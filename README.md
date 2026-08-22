# taffle

Convert audiobooks and music (m4b/m4a, mp3, opus/ogg, flac, wav) into Tonie Audio
Format (TAF) files for use with teddycloud-backed Tonieboxes. One direct transcode
(source → Opus), sample-accurate chapters, silence trimming and pause normalization,
and a built-in TAF validator (`taffle info`).

```console
$ taffle book.m4b
wrote book.taf (1:04:12, 16 chapters)
wrote book.jpg
```

## Install

```sh
cargo install taffle-cli
```

The crate is named `taffle-cli`; the binary it installs is called `taffle`. From a
clone — which is where it lives until the crates are published:

```sh
cargo install --path crates/taffle-cli
```

Building needs a C toolchain and **libopus** — `libopus-dev` on Debian and Ubuntu,
`opus` on Homebrew, `opus-dev` on Alpine. The Opus bindings look the library up with
`pkg-config` and fall back to building their own vendored copy with CMake, which is
slower and wants a CMake older than 4; installing the system library is the shorter
road. Nothing is needed at runtime.

## Usage

```
taffle <INPUT>... [options]          # convert to TAF
taffle info <FILE.taf>...            # inspect/validate TAF files

Options (convert):
  -o, --output <PATH>                Output .taf. Default: first input's name + .taf
      --skip-leading <SECONDS>       Drop N seconds from the very start (e.g. 4.4)
      --trim-pause-leading           Trim leading silence at the start of chapter 1
                                     (applied after --skip-leading)
      --trim-pause-each-chapter      Trim leading silence at the start of every
                                     chapter (implies chapter 1 too)
      --add-pause-leading <SECONDS>  Insert silence at the start of chapter 1
                                     (after any trimming)
      --add-pause-each-chapter <SECONDS>
                                     Insert silence at the start of every chapter
      --chapters <LIST>              Override chapter marks
                                     ("0:00,12:34,1:02:10.5")
      --no-cover                     Don't extract embedded cover art
```

`taffle --help` states the same thing at length, including what stacks with what.

### Converting

One book, written beside itself as `book.taf`, with its cover art beside that as
`book.jpg` or `book.png`:

```sh
taffle book.m4b
```

Several files are one book. They play in the order they are named, and each of them
begins a chapter:

```sh
taffle 01.mp3 02.mp3 03.mp3 -o album.taf
```

An m4b's own chapter marks are kept where it carries any. `--chapters` overrides
whatever the inputs carry, in the formats `SS(.ms)`, `MM:SS(.ms)` and `HH:MM:SS(.ms)`:

```sh
taffle book.m4b --chapters 0:00,12:34,1:02:10.5
```

A Toniebox plays 99 chapters. More than that is a warning and not a refusal — the
file is written all the same.

### Normalizing the pause a book opens with

Publisher intros, a beat of silence, half a second of nothing: books do not begin the
same way twice. The three operations run in the order they are stated — drop, then
trim, then insert — so this is exactly one second of silence in front of the first
sound of the book, whatever was in front of it before:

```sh
taffle book.m4b --skip-leading 4.4 --trim-pause-leading --add-pause-leading 1.0
```

`--trim-pause-each-chapter` and `--add-pause-each-chapter` do the same for every
chapter. At chapter 1 the two `--add-` options add up: each of them states what it puts
in, and neither takes the other's place.

### Reading a TAF back

`taffle info` reads a file the way a Toniebox reads one — the header block, then the
audio region one 4096-byte block at a time, hashed as it goes — and prints what the
file holds once it has been found to hold it:

```console
$ taffle info book.taf
book.taf
  audio id: 1787235111
  duration: 1:04:12
  audio: 52588544 bytes
  chapters: 16
      #  block  start
      1      0   0:00
      2    648   3:15
      3   1326   6:38
      4   2084  10:26
      5   2784  13:56
      6   3505  17:32
      7   4566  22:50
      8   5313  26:34
      9   5974  29:53
     10   7064  35:19
     11   7717  38:35
     12   8522  42:37
     13   9586  47:56
     14  10312  51:34
     15  10945  54:43
     16  11681  58:24
  valid
```

The block is where a chapter starts in the file — block *n* begins at offset
`4096 * (n + 1)` — which is what a box seeks on and what the header holds. A file that
is not the one its header describes gets no report: it is said on stderr instead,
naming the file and what went wrong, and the exit code is 1.

```console
$ taffle info sine.taf broken.taf; echo "exit $?"
sine.taf
  audio id: 444913029
  duration: 0:09
  audio: 110592 bytes
  chapters: 1
      #  block  start
      1      0   0:00
  valid
broken.taf: the audio does not hash to the sha1 its header states: header 1bc822ae550f04dce13ed223bba9927fb845a1f4, audio 1ac822ae550f04dce13ed223bba9927fb845a1f4
error: 1 file is not the TAF its header describes
exit 1
```

Every file named is read, whatever the ones in front of it came to, so a run over a
directory of books says what is wrong with each of them in one go. The code is 0 only
if every file validated — structure *and* SHA-1.

## The crates

| Crate | What it is |
|---|---|
| [`taf`](crates/taf) | The Tonie Audio Format itself: header codec, Ogg framing, packet padding, reader and writer. `no_std`, no dependencies, hashing inverted to a trait — small enough for the box's own microcontroller. |
| [`taf-encode`](crates/taf-encode) | The conversion engine: decode (symphonia, libopus), resample to 48 kHz stereo, trim and pad silence, resolve chapters, encode Opus across every core into `taf`'s writer — deterministically: the file is the audio's, not the machine's. |
| [`taffle`](crates/taffle) | The application layer a frontend sits on: paths, the output name, the cover beside the file, progress — and reading a TAF back, block by block, to say whether it holds. |
| [`taffle-cli`](crates/taffle-cli) | The `taffle` binary — argument parsing, progress, error rendering, and nothing else. |

`crates/taf/FORMAT.md` describes the file format in full and cites teddycloud for
every constant it states.

## License

Licensed under the MIT license ([LICENSE](LICENSE)).
