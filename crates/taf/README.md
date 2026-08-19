# taf

The Tonie Audio Format itself — header codec, Ogg framing, packet padding, and reader/writer
interfaces — with nothing about how audio gets into a TAF file.

A TAF is what a [teddycloud](https://github.com/toniebox-reverse-engineering/teddycloud)-backed
Toniebox plays: a 4096-byte header block stating the audio region's SHA-1, its length, the file's
audio id and its chapter starts, and behind that block an Ogg-encapsulated Opus stream — 48 kHz,
stereo, one packet per 60 ms — laid out on the same 4096-byte grid. From file offset 8192 on,
every block boundary starts exactly one page of exactly one block, which is what lets a box seek to
a chapter by multiplying. [`FORMAT.md`](FORMAT.md) describes the format in full, cites teddycloud
for every constant, is backed by committed fixtures, and is authoritative.

`#![no_std]` and **zero dependencies**, by dependency inversion: what the published specs define,
this crate implements; what is a general-purpose capability — hashing, and Opus encoding itself —
the caller brings.

```toml
[dependencies]
taf = "0.1"
```

## Features

| Feature | What it adds |
| --- | --- |
| *(none)* | Everything that reads a file where it lies, without an allocator: `HeaderView` borrows the block it parses, `PageView` slices packets out of the page they sit in, `Validator` checks an audio region one block at a time, and `encode_header`, `opus_head` and `opus_tags` write into fixed-size arrays. |
| `alloc` | Everything that builds bytes rather than borrowing them: `opus_packet::pad_to`, `ogg::PageBuilder` and `writer::TafWriter`, whose pages are as long as the packets on them make them. |
| `std` *(default)* | `std::error::Error` on every error type, and `writer::write_taf`, which writes a whole file — header block last — into anything that can be written to and seeked in. |

## Bring your own SHA-1

A TAF header carries a SHA-1 of the audio region, so writing and validating files needs a digest —
but requiring a *particular* one would cost this crate its zero-dependency, `no_std` footing. The
`Sha1` trait states what is needed and the caller supplies it: a software crate on a host, a
hardware peripheral on a microcontroller. Here it is over the RustCrypto `sha1` crate:

```rust
use taf::digest::Sha1;

struct Digest(sha1::Sha1);

impl Sha1 for Digest {
    fn update(&mut self, data: &[u8]) {
        sha1::Digest::update(&mut self.0, data);
    }

    fn finalize(self) -> [u8; 20] {
        sha1::Digest::finalize(self.0).into()
    }
}

let mut digest = Digest(<sha1::Sha1 as sha1::Digest>::new());
// RFC 3174 SHA-1 is the whole contract, whether a message arrives in one call or several.
Sha1::update(&mut digest, b"a");
Sha1::update(&mut digest, b"bc");

let hex: String = digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
```

That digest is what `writer::TafWriter` hashes a file with as it writes it, and what a
`reader::Validator` is fed the blocks of; a validator handed no digest at all checks the file's
structure and leaves its hash alone.

## License

Licensed under the MIT license ([LICENSE](../../LICENSE)).
