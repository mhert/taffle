# TAF — Toniebox Audio File format

Authoritative format description for the `taf` crate. Every constant below is backed by a
citation into the teddycloud sources **and**, where observable, by a byte range in the
committed fixtures under `crates/taf/tests/fixtures/`. Points that are easy to get wrong from
a cursory reading of the format are listed in [Format gotchas](#format-gotchas).

## Sources

| Source | Pinned revision |
| --- | --- |
| `toniebox-reverse-engineering/teddycloud` @ `master` | commit `3b69cbe482a3d1553252dddea8d5b557312ec6c6` (fetched 2026-08-18) |
| `toniebox-reverse-engineering/teddycloud` @ tag `tc_v0.6.2` | commit `203f12d3d357d16268b83c7bdedb332820b9e87a` |
| Fixture producer image | `ghcr.io/toniebox-reverse-engineering/teddycloud:tc_v0.6.2_ubuntu`, digest `sha256:776b42c40728c6d3251d502031d7c1852eef578bed424a2ea662edf1da952e48` |

File/line citations below refer to the `master` commit. The `tc_v0.6.2` tag — the version
that produced every committed fixture — is byte-for-byte identical to `master` in all
format-relevant code. The only differences in `src/toniefile.c` are `TRACE_*` format-string
changes (`%zu` → `%" PRIuSIZE "`) and a post-write validation call added to
`toniefile_close()` on master; `include/toniefile.h` and
`proto/toniebox.pb.taf-header.proto` differ only in an unrelated struct field and a trailing
newline. Line numbers in `src/toniefile.c` after ~line 362 are shifted by +5 on master
relative to `tc_v0.6.2`.

The proto lives at `proto/toniebox.pb.taf-header.proto` in the repository root, **not** at
`src/proto/…` (that path 404s; `src/proto/proto/` holds the generated `*.pb-c.[ch]`).
Working raw URL:
`https://raw.githubusercontent.com/toniebox-reverse-engineering/teddycloud/master/proto/toniebox.pb.taf-header.proto`

## File layout

```
offset 0      ┌──────────────────────────────────────────────┐
              │ 4-byte big-endian protobuf length prefix     │  header block
              │ protobuf TonieboxAudioFileHeader (= prefix)  │  exactly 4096 bytes
              │ [zero padding, 0..1 bytes, see wobble]       │
offset 4096   ├──────────────────────────────────────────────┤
              │ Ogg page 0: OpusHead packet   (47 bytes)     │
offset 4143   │ Ogg page 1: OpusTags packet   (465 bytes)    │  first block of audio
offset 4608   │ Ogg page 2: audio            (3584 bytes)    │  = 4096 bytes total
offset 8192   ├──────────────────────────────────────────────┤
              │ Ogg page 3: audio, exactly 4096 bytes        │
offset 12288  │ Ogg page 4: audio, exactly 4096 bytes        │
              │ …                                            │
              └──────────────────────────────────────────────┘  no EOS page
```

Total file size is always a multiple of 4096. `TONIEFILE_FRAME_SIZE = 4096`
(`include/toniefile.h:14`) is the block granularity for both the header and every audio page
from offset 8192 on.

## Header block (bytes 0..4096)

### Length prefix

Bytes 0..4 are the protobuf length, big-endian (`src/toniefile.c:336-344`,
`proto_be[0] = proto_size >> 24; …`). Observed in all three fixtures: `00 00 0f fc` = **4092**.

4092 = `TONIEFILE_FRAME_SIZE - 4` (`src/toniefile.c:60`). teddycloud's own validator accepts
`protobufSize <= TAF_HEADER_SIZE` where `#define TAF_HEADER_SIZE 4092` (`include/handler.h:98`,
check at `src/handler.c:681`). A parser must therefore **read the prefix and bounds-check it
against 4092**, not assume equality — see [Fill sizing](#fill-sizing-and-the-1-varint-wobble)
for the one case where teddycloud emits 4091.

### Protobuf message

Verbatim from `proto/toniebox.pb.taf-header.proto`:

```proto
syntax = "proto2";

message TonieboxAudioFileHeader {
  required bytes sha1_hash = 1;
  required uint64 num_bytes = 2;
  required uint32 audio_id = 3;
  repeated uint32 track_page_nums = 4 [packed=true];
  required bytes _fill = 5;
  //custom_fields_start
  optional uint64 ogg_granule_position = 6;
  optional uint64 ogg_packet_count = 7;
  optional uint64 taf_block_num = 8;
  optional uint64 pageno = 9;
  //custom_fields_end
}
```

Fields are emitted in field-number order 1..9. Observed tag bytes, in order, in all three
fixtures: `0x0a, 0x10, 0x18, 0x22, 0x2a, 0x30, 0x38, 0x40, 0x48`.

**Fields 6–9 are always present in teddycloud-written files** (`src/toniefile.c:323-330` sets
`has_* = true` unconditionally). They are teddycloud's own resume/append bookkeeping, not part
of the box-facing format; a reader must skip unknown/optional fields by wire type rather than
stopping at field 5.

| # | Name | Wire | Semantics |
| --- | --- | --- | --- |
| 1 | `sha1_hash` | len-delimited | SHA-1 (20 bytes) over the entire audio region, i.e. all bytes from offset 4096 to EOF (`src/toniefile.c:258-259, 517-518, 356-359`). Length is validated as exactly 20 (`src/handler.c:689`). |
| 2 | `num_bytes` | varint (uint64) | Length of the audio region in bytes = file size − 4096 (`src/toniefile.c:358`, `ctx->taf.num_bytes = ctx->audio_length`). Always a multiple of 4096. Bounded by `TONIE_LENGTH_MAX = INT32_MAX - 0x1000 = 2 147 479 551` (`include/net_config.h:63-65`) — the placeholder written before `toniefile_close` (`src/toniefile.c:103, 112`). |
| 3 | `audio_id` | varint (uint32) | Content id; also the Ogg stream serial number (`src/toniefile.c:169`, `ogg_stream_init(&ctx->os, audio_id)`). teddycloud sets it to `time(NULL) - TEDDY_BENCH_AUDIO_ID_DEDUCT` with `TEDDY_BENCH_AUDIO_ID_DEDUCT = 0x50000000` (`include/toniefile.h:30`, `src/toniefile.c:716`). |
| 4 | `track_page_nums` | packed varints | Chapter starts as **TAF block indices**, 0-based, counted from the first audio block at file offset 4096 (`src/toniefile.c:393`, `track_page_nums[n++] = ctx->taf_block_num`). Block *n* starts at file offset `4096 * (n + 1)`. Always starts with `0` (`src/toniefile.c:307` creates chapter 0 during `toniefile_create`). **Not** Ogg page sequence numbers — see [Format gotchas](#format-gotchas). |
| 5 | `_fill` | len-delimited | Zero bytes that pad the message so the block is exactly 4096. Never carries information; observed all-zero in every fixture. |
| 6 | `ogg_granule_position` | varint | Granule of the last packet *accepted by the encoder*, including packets still buffered when the file was closed. Larger than the last written page's granule — see [Tail truncation](#tail-truncation). |
| 7 | `ogg_packet_count` | varint | Packets handed to libogg, including the 2 header packets and the buffered tail. |
| 8 | `taf_block_num` | varint | Number of 4096-byte audio blocks written = `num_bytes / 4096`. |
| 9 | `pageno` | varint | Next Ogg page sequence number = number of pages written. |

Annotated header block of `golden-sine.taf` (offsets are decimal):

```
   0  00 00 0f fc                 length prefix = 4092
   4  0a 14                       field 1, wire 2, len 20
   6  1a c8 22 ae … a1 f4         sha1_hash (20 bytes)
  26  10 80 e0 06                 field 2, num_bytes    = 110592
  30  18 85 ab 93 d4 01           field 3, audio_id     = 444913029
  36  22 01 00                    field 4, len 1, track_page_nums = [0]
  39  2a cb 1f                    field 5, len 4043
  42  00 … 00                     _fill payload, 4043 zero bytes, ends at 4085
4085  30 80 97 1d                 field 6, ogg_granule_position = 478080
4089  38 a8 01                    field 7, ogg_packet_count     = 168
4092  40 1b                       field 8, taf_block_num        = 27
4094  48 1d                       field 9, pageno               = 29
4096                              end of block, no trailing pad byte
```

Note that `_fill` sits *between* the required fields and the optional ones — the message is not
"fields then padding at the end", so a reader must keep parsing past field 5.

`real-header-1.bin` shows the multi-chapter case: `22 03 00 1b 37` → `[0, 27, 55]`.
`real-header-2.bin`: `22 03 00 97 05` → `[0, 663]`.

### Fill sizing and the ±1 varint wobble

`toniefile_header()` (`src/toniefile.c:58-93`) sizes `_fill` so the packed message is exactly
`proto_frame_size = 4092` bytes:

1. Set `_fill.len = 4092`, pack-size the message → `d1`.
2. Shrink: `_fill.len = 4092 + (4092 - d1)`, i.e. subtract the overshoot `d1 - 4092`.
3. Re-measure → `d2`.
4. If `d2 == 4093`, decrement `_fill.len` by one.
5. Accept only `d2 == 4092` or `d2 == 4091`; otherwise log an error and write nothing.

Why the wobble exists: shrinking `_fill` by *k* bytes shrinks the message by *k* — unless the
length varint of `_fill` itself changes size. With `_fill.len` in the normal range
(128..16383) it is a 2-byte varint before and after, so step 3 lands exactly on 4092 (all
three fixtures: `_fill.len` = 4043 / 4041 / 4037, prefix 4092). Only when the other fields grow
past ~3964 bytes (a very long chapter list) does `_fill.len` drop below 128, its varint shrinks
to 1 byte, and the message packs to **4091**. Then the header block is 4 + 4091 = 4095 written
bytes plus one trailing zero byte, since the writer seeks to 4096 before writing audio
(`src/toniefile.c:151`).

Step 4 is dead code: `d2` is only ever 4092 or 4091, and the decrement it performs is not
reflected in the `d2` that step 5 tests, so a hypothetical 4093 would be rejected rather than
fixed. Do not model an implementation on it.

Implementation guidance for the encoder side: compute the fill length so the packed message is
exactly 4092, and when that is impossible fall back to 4091 + one zero pad byte. A decoder
must accept both.

Known teddycloud edge case, worth *not* reproducing: `isValidTaf` hashes from offset
`4 + prefix` (`src/handler.c:677-683`) while the writer hashes from 4096. These coincide at
prefix 4092 but not at 4091. Hash from **4096** (block-aligned), which matches the writer and
all observed files.

### Chapter count limit

`#define TONIEFILE_MAX_CHAPTERS 100` (`include/toniefile.h:15`), but the guard is
`if (n_track_page_nums >= TONIEFILE_MAX_CHAPTERS - 1) return ERROR_FAILURE;`
(`src/toniefile.c:389`), so the 100th chapter is rejected: **99 chapters maximum**.
Independently, the CLI refuses more than 99 source files
(`src/main.c:426-429`, "Not more than 99 source files allowed!") and the multi-source
buffers are `char source[99][PATH_LEN]` (`include/toniefile.h:68-69`, `src/main.c:254`).

The header block imposes a second, softer limit: the packed chapter list plus the fixed fields
must fit in 4092 bytes. With ~35 bytes of fixed fields that is thousands of entries, so 99 is
the binding limit in practice.

## Audio region (offset 4096 to EOF)

Standard Ogg-encapsulated Opus, with TAF-specific block alignment.

### Opus encoder settings

| Setting | Value | Source |
| --- | --- | --- |
| Sample rate | 48000 Hz | `OPUS_SAMPLING_RATE` (`include/toniefile.h:7`), `opus_encoder_create` (`src/toniefile.c:154`) |
| Channels | 2 | `OPUS_CHANNELS` (`include/toniefile.h:10`) |
| Application | `OPUS_APPLICATION_AUDIO` | `src/toniefile.c:154` |
| Frame duration | 60 ms (`OPUS_FRAMESIZE_60_MS`) | `OPUS_FRAME_SIZE_MS` (`include/toniefile.h:6`), `OPUS_SET_EXPERT_FRAME_DURATION` (`src/toniefile.c:166`) |
| Samples per frame | **2880** per channel (`48000 * 60 / 1000`) | `OPUS_FRAME_SIZE` (`include/toniefile.h:9`) |
| VBR | on | `OPUS_SET_VBR(1)` (`src/toniefile.c:165`) |
| Bitrate | `encode.bitrate * 1000`, default **96 kbit/s** | `src/toniefile.c:164`; default and range 0..256 at `src/settings.c:255` |
| Packet padding threshold | 64 bytes | `OPUS_PACKET_PAD` / `OPUS_PACKET_MINSIZE` (`include/toniefile.h:11-12`) |

Observed: every page's granule advances by a multiple of 2880 (6–15 packets per page across
the fixtures), confirming a uniform 60 ms frame size.

### OpusHead packet (19 bytes)

Built at `src/toniefile.c:172-180`. Observed verbatim at file offset 4124 (page 0 body: 4096 +
27 header + 1 lacing byte) in all three fixture families:

```
4f 70 75 73 48 65 61 64  "OpusHead"
01                       version 1
02                       channel count 2
38 01                    pre-skip 312 (0x0138, little-endian)
80 bb 00 00              input sample rate 48000
00 00                    output gain 0
00                       channel mapping family 0
```

The pre-skip **312** is hard-coded in teddycloud (`0x38, 0x01`), matching libopus's 6.5 ms
encoder lookahead at 48 kHz. It is not derived from the encoder at runtime.

### OpusTags packet (always 436 bytes)

`unsigned char comment_data[0x1B4];` — a fixed **436-byte** buffer (`src/toniefile.c:182`),
pre-filled with ASCII `'0'` (0x30) (`src/toniefile.c:185`). Layout:

```
"OpusTags"                              8 bytes
u32-le vendor length, vendor string      "teddyCloud" (10 bytes)
u32-le comment count = 2
u32-le len, "version=<BUILD_FULL_NAME_LONG>"
u32-le len, "pad=" + '0' * (len - 4)     absorbs the remaining space
```

The pad comment length is computed as `remain = 436 - pos - 4` (`src/toniefile.c:213-216`), so
the packet is **always exactly 436 bytes** regardless of how long the version string is.

Observed in `golden-sine.taf` at file offset 4172 (page 1 body: 4143 + 27 header + 2 lacing
bytes): vendor `teddyCloud`, 2 comments,
`version=TeddyCloud v0.6.2 (203f12d) - 2024-10-26 18:14:34 +0000 ubuntu linux-x86_64(64)`
(87 bytes) and a 315-byte `pad=` comment whose payload is 311 `'0'` characters. The embedded
commit `203f12d` matches the `tc_v0.6.2` tag commit, tying the fixtures to the pinned source.

### Page layout and block alignment

The header packets are flushed as two pages before any audio (`src/toniefile.c:243-260`):

| Page | File offset | Bytes | Composition |
| --- | --- | --- | --- |
| 0 | 4096 | 47 | 27 header + 1 lacing + 19 body (OpusHead), BOS flag `0x02` |
| 1 | 4143 | 465 | 27 header + 2 lacing + 436 body (OpusTags; 436 = 255 + 181 → 2 segments) |
| 2 | 4608 | 3584 | first audio page, sized to end exactly at 8192 |
| ≥3 | 8192 + 4096·k | 4096 | one page per block, each exactly 4096 bytes |

47 + 465 = **512**, which is why the 436-byte OpusTags target exists: it leaves the first audio
page exactly 3584 bytes to close the block. From offset 8192 on, **every 4096-byte boundary is
the start of exactly one page whose total length is exactly 4096** — verified over all 1328
aligned pages of the 5.4 MB two-chapter file and all 26 of the golden file.

Chapter block *n* ⇒ the page at file offset `4096 * (n + 1)`, whose Ogg sequence number is
`n + 2` (block 0 holds pages 0, 1, 2). Observed: chapters `[0, 27, 55]` → pages 2, 29, 57;
chapters `[0, 663]` → pages 2, 665.

Further observed invariants (all fixtures): Ogg version byte 0; page flags only `0x02` (BOS,
page 0) and `0x00`; **no continued-packet flag (`0x01`) anywhere**, i.e. packets never span
pages; serial number == `audio_id` on every page; sequence numbers contiguous from 0; every
page CRC valid.

### The exact page-fill algorithm

Per 60 ms frame (`src/toniefile.c:421-465`), where `pending_body`/`pending_lacing` are the
bytes and lacing values already buffered for the current page:

```
page_used     = (file_pos % 4096) + 27 + pending_lacing + pending_body
page_remain   = 4096 - page_used
frame_payload = (page_remain / 256) * 255 + (page_remain % 256) - 1
reconstructed = (frame_payload / 255) + 1 + frame_payload
if (page_remain != reconstructed && frame_payload > 64) frame_payload -= 64
if (frame_payload < 63) -> error: not enough space in this block
frame_len = opus_encode(..., max_data_bytes = frame_payload)
if (frame_payload - frame_len < 64) opus_packet_pad(frame, frame_len, frame_payload)
```

`frame_payload` is the largest packet size `B` satisfying `B + (B / 255 + 1) <= page_remain`
(each 255-byte run costs one lacing byte). The `reconstructed != page_remain` case means a
1-byte gap that no packet size can fill; backing off by 64 bytes leaves room for one more
segment. The final packet of a page is padded via RFC 6716 packet padding
(`opus_packet_pad`) so the page lands on the boundary exactly.

After the packet is queued (`src/toniefile.c:488-529`): recompute `page_remain`; if it is
`< TONIEFILE_PAD_END` (64) it must be exactly 0, else teddycloud aborts with "unexpected small
padding". At 0 the page is flushed and written, and `taf_block_num` is incremented for each
4096 boundary crossed, with a hard error if the write did not land on a boundary.

Worked example, golden fixture page 2 (`file_pos = 4608`): `page_used = 512 + 27 = 539`,
`page_remain = 3557`, `frame_payload = 13*255 + 229 - 1 = 3543`, `reconstructed = 3557` (no
back-off) → first packet 895 bytes; the page ends up with packets `[895, 716, 743, 738, 450]`
(3542 body bytes, 15 lacing values): `512 + 27 + 15 + 3542 = 4096`. The last packet was padded
to exactly the 450 bytes that were left.

### Tail truncation and the missing EOS page

`toniefile_close()` (`src/toniefile.c:354-378`) never flushes libogg, so packets still buffered
when the last block could not be completed are **dropped**, and **no page carries the EOS flag
`0x04`** — TAF streams simply stop at a block boundary. All packets are created with
`e_o_s = 0` (`src/toniefile.c:480`).

Consequences, observed in `golden-sine.taf` (10.00 s of input): last written page granule
463680 = 9.66 s, while header field 6 `ogg_granule_position` = 478080 = 9.96 s — 5 encoded
packets (0.30 s) were buffered and lost. Likewise field 7 `ogg_packet_count` = 168 versus 163
packets actually present (2 header + 161 audio). A validator must **not** require an EOS page
and must expect the last page's granule to be ≤ header field 6.

## Validation rules

`isValidTaf` (`src/handler.c:669`, called from `toniefile_close` on master) checks:

1. 4-byte BE prefix readable and `<= 4092`;
2. protobuf unpacks;
3. `sha1_hash.len == 20`;
4. SHA-1 over the audio region equals `sha1_hash`;
5. audio region byte count equals `num_bytes`.

Steps 4 and 5 were added after `tc_v0.6.2` (which validated only 1–3), but the writer computes
both fields identically in both versions — the golden fixture satisfies all five, verified
independently.

## Constants summary

| Constant | Value | Source |
| --- | --- | --- |
| Block / page size | 4096 | `TONIEFILE_FRAME_SIZE`, `include/toniefile.h:14` |
| Protobuf region size | 4092 (4091 in the wobble case) | `src/toniefile.c:60`, `TAF_HEADER_SIZE`, `include/handler.h:98` |
| Length prefix | 4 bytes, big-endian | `src/toniefile.c:336-344` |
| Sample rate | 48000 Hz | `include/toniefile.h:7` |
| Channels | 2 | `include/toniefile.h:10` |
| Frame duration | 60 ms | `include/toniefile.h:6` |
| Samples per frame | 2880 | `include/toniefile.h:9` |
| Bitrate | 96 kbit/s default, VBR | `src/settings.c:255`, `src/toniefile.c:164-165` |
| OpusHead pre-skip | 312 | `src/toniefile.c:176` |
| OpusHead size | 19 bytes | `src/toniefile.c:172-180` |
| OpusTags size | 436 (0x1B4) bytes | `src/toniefile.c:182` |
| Header pages total | 512 bytes (47 + 465) | observed, all fixtures |
| First audio page | offset 4608, 3584 bytes | observed, all fixtures |
| First block-aligned page | offset 8192 | observed, all fixtures |
| Ogg header length | 27 bytes | `OGG_HEADER_LENGTH`, `include/toniefile.h:18` |
| Page end padding threshold | 64 | `TONIEFILE_PAD_END`, `include/toniefile.h:16` |
| Opus packet pad / min size | 64 | `include/toniefile.h:11-12` |
| Max chapters | 99 (constant is 100, guard is `>= 100 - 1`) | `include/toniefile.h:15`, `src/toniefile.c:389`, `src/main.c:426-429` |
| Max audio length | 2 147 479 551 bytes | `include/net_config.h:63-65` |
| Audio id derivation | `time(NULL) - 0x50000000` | `include/toniefile.h:30`, `src/toniefile.c:716` |

## Format gotchas

Five points here are easy to get wrong from a cursory reading of the format. This document is
authoritative:

1. **Proto field names.** Upstream names are `sha1_hash`/`num_bytes`/`audio_id`/
   `track_page_nums`/`_fill`. Wire layout is what matters; the Rust API may use whichever field
   names read better as long as the wire mapping matches the table above.
2. **`num_bytes` is `uint64`, not `uint32`** in the proto. Values are bounded by
   `INT32_MAX - 4096`, so a `u32` in-memory representation is safe, but the varint decoder must
   tolerate a full 64-bit varint on the wire rather than rejecting it.
3. **`track_page_nums` are TAF block indices, not Ogg page sequence numbers.** Block *n* lives
   at file offset `4096 * (n + 1)` and is Ogg page `n + 2`. Chapter 0 is block 0 = page 2.
4. **The first audio page is at offset 4608, not 8192.** The two header pages occupy only
   4096..4608; the first audio page (3584 bytes) fills the rest of the block — OpusHead plus
   padded OpusTags do not fill all of 4096..8192. What *is* true from 8192 on: every 4096-byte
   boundary starts a page of exactly 4096 bytes.
5. **There is no EOS page** and the audio tail (up to one block) is dropped by teddycloud on
   close. A validator that requires an EOS flag rejects every real TAF.

Additionally, fields 6–9 (`ogg_granule_position`, `ogg_packet_count`, `taf_block_num`,
`pageno`) are always present in teddycloud output. Readers must skip them; a writer may omit
them (nothing in teddycloud requires them except its own append path), but must then size
`_fill` accordingly.

## Fixture cross-check

Values observed by hexdump/parse of the committed fixtures (see
`crates/taf/tests/fixtures/README.md` for provenance):

| | `golden-sine.taf` | `real-header-1.bin` | `real-header-2.bin` |
| --- | --- | --- | --- |
| Size on disk | 114 688 (28 blocks) | 4096 (header slice) | 4096 (header slice) |
| Prefix | `00 00 0f fc` (4092) | `00 00 0f fc` | `00 00 0f fc` |
| Tag order | `0a 10 18 22 2a 30 38 40 48` | same | same |
| `sha1_hash` | `1ac822ae550f04dce13ed223bba9927fb845a1f4` | `6cdb1e91…` | `4adb8a3f…` |
| `num_bytes` | 110 592 (= size − 4096) | 339 968 | 5 443 584 |
| `audio_id` | 444 913 029 | 444 913 053 | 444 913 094 |
| `track_page_nums` | `[0]` | `[0, 27, 55]` | `[0, 663]` |
| `_fill` length | 4043 (all zero) | 4041 (all zero) | 4037 (all zero) |
| Fields 6/7/8/9 | 478080 / 168 / 27 / 29 | 1440000 / 502 / 83 / 85 | 23040000 / 8002 / 1329 / 1331 |

Full-file checks on `golden-sine.taf` (and, before slicing, on the two source files the header
fixtures came from):

- SHA-1 over bytes 4096..EOF equals `sha1_hash`; `num_bytes` equals the audio region length.
- 29 pages, all CRCs valid, all serials == `audio_id`, sequence numbers 0..28 contiguous.
- OggS magic at 4096, 4143, 4608, 8192, and at every 4096 boundary from 8192 on (26 of them);
  each of those aligned pages is exactly 4096 bytes.
- OpusHead bytes exactly as listed above (pre-skip 312); OpusTags exactly 436 bytes with the
  `pad=` comment of 315 bytes.
- No page has the EOS (`0x04`) or continued (`0x01`) flag; only page 0 has BOS (`0x02`).
- `ffprobe` on bytes 4096.. reports `ogg`/`opus`, 48000 Hz, stereo, 9.66 s for a 10 s input.
