//! The Tonie Audio Format itself — header codec, Ogg framing, packet padding, and
//! reader/writer interfaces — with nothing about how audio gets into a TAF file.
//!
//! # The format in short
//!
//! A TAF is a 4096-byte header block and, behind it, an Ogg-encapsulated Opus stream — 48 kHz,
//! stereo, one packet per 60 ms — laid out on that same 4096-byte grid.
//!
//! The header block is a four-byte big-endian length prefix, a protobuf message, and zero fill out
//! to the end of the block. The message states the SHA-1 of everything behind the block, that
//! region's length in bytes, the file's audio id, and the blocks its chapters start at.
//!
//! The audio region starts at file offset 4096. The two pages carrying `OpusHead` and `OpusTags`
//! span its first 512 bytes, one audio page closes the block they share, and from file offset 8192
//! on every 4096-byte boundary starts exactly one page of exactly 4096 bytes. That grid is what a
//! box seeks on: a chapter is a *block index*, and block `n` begins at file offset `4096 * (n + 1)`
//! — plus 512 for the first chapter, whose block holds the two header pages as well. Every page
//! states the file's audio id as its serial number, no page states the continued-packet flag, and
//! no page ends the stream: a TAF stops at a block boundary and never carries EOS.
//!
//! `FORMAT.md` in this crate describes all of that in full, cites teddycloud for every constant,
//! and is authoritative.
//!
//! # What is implemented here, and what is not
//!
//! **In-crate** if the published format specs (RFC 3533 Ogg, RFC 7845 Ogg-Opus, RFC 6716 Opus
//! framing, the protobuf wire format) fully define it *and* TAF's own invariants depend on it —
//! which is why Ogg page framing (including the Ogg CRC-32), Opus packet padding, and the header
//! codec are implemented here. **Behind an interface** if it is a general-purpose capability with
//! genuinely interchangeable implementations — which is why hashing (SHA-1) and Opus
//! encoding/decoding (DSP) stay out.
//!
//! That is what leaves the crate with no dependencies of its own: [`digest::Sha1`] states what a
//! file's hash has to come from and the caller brings the implementation — a software crate on a
//! host, a hardware peripheral on a microcontroller — and Opus packets are the caller's to encode.
//!
//! # Features
//!
//! | Feature | What it adds |
//! | --- | --- |
//! | *(none)* | Everything that reads a file where it lies, without an allocator: [`header::HeaderView`] borrows the block it parses, [`ogg::PageView`] slices packets out of the page they sit in, [`reader::Validator`] checks an audio region one block at a time, and [`header::encode_header`], [`ogg::opus_head`] and [`ogg::opus_tags`] write into fixed-size arrays. |
//! | `alloc` | Everything that builds bytes rather than borrowing them: [`opus_packet::pad_to`], [`ogg::PageBuilder`] and [`writer::TafWriter`], whose pages are as long as the packets on them make them. |
//! | `std` *(default)* | `std::error::Error` on every error type, and [`writer::write_taf`], which writes a whole file — header block last — into anything that can be written to and seeked in. |
//!
//! The core is what an ESP32 runs: nothing there allocates, and a file of any length is read
//! through the one 4096-byte block the caller happens to be holding.

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod digest;
pub mod header;
pub mod id;
pub mod ogg;
#[cfg(feature = "alloc")]
pub mod opus_packet;
pub mod reader;
#[cfg(feature = "alloc")]
pub mod writer;

/// The crate's README, compiled as a doctest so the example in it cannot go stale.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct Readme;
