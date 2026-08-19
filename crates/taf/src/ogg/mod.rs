//! The Ogg framing that carries a TAF's audio, read where it lies.
//!
//! RFC 3533 is the framing itself; what makes it TAF is the alignment. The two pages that carry
//! the Opus headers and the first audio page share the first block of the audio region, and from
//! file offset 8192 on every 4096-byte block holds exactly one page of exactly [`PAGE_LEN`]
//! bytes — which is what lets a box seek to a chapter by multiplying. `FORMAT.md` in this crate
//! describes the layout and is authoritative.

mod crc;
mod page;

pub use page::{Packets, PageError, PageView};

use crate::header::BLOCK_LEN;

/// The length of an aligned Ogg page, which is exactly one [`BLOCK_LEN`] block.
///
/// teddycloud fills every page it writes out to a whole block, padding the last Opus packet of
/// the page to land on the boundary. Only the three pages of the first block are shorter: the two
/// Opus header pages, and the first audio page, which is sized to close that block.
pub const PAGE_LEN: usize = BLOCK_LEN;
