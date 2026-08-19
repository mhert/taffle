//! The Ogg framing that carries a TAF's audio, read where it lies.
//!
//! RFC 3533 is the framing itself; what makes it TAF is the alignment. The two pages that carry
//! the Opus headers and the first audio page share the first block of the audio region, and from
//! file offset 8192 on every 4096-byte block holds exactly one page of exactly [`PAGE_LEN`]
//! bytes — which is what lets a box seek to a chapter by multiplying. `FORMAT.md` in this crate
//! describes the layout and is authoritative.

mod build;
mod crc;
mod page;

#[cfg(feature = "alloc")]
pub use build::PageBuilder;
pub use build::{opus_head, opus_tags, BuildError, OPUS_HEAD_LEN, OPUS_PRE_SKIP, OPUS_TAGS_LEN};
pub use page::{Packets, PageError, PageView};

/// The page arithmetic the writer sizes its packets with, which is this module's own.
#[cfg(feature = "alloc")]
pub(crate) use build::{packet_cost, MAX_SEGMENTS, SEGMENT_LEN};

use crate::header::BLOCK_LEN;

/// The length of an aligned Ogg page, which is exactly one [`BLOCK_LEN`] block.
///
/// teddycloud fills every page it writes out to a whole block, padding the last Opus packet of
/// the page to land on the boundary. Only the three pages of the first block are shorter: the two
/// Opus header pages, and the first audio page, which is sized to close that block.
pub const PAGE_LEN: usize = BLOCK_LEN;

/// The bytes RFC 3533 puts in front of a page's lacing table: the capture pattern, the version,
/// the type flags, the granule position, the serial number, the sequence number, the checksum,
/// and how many lacing values follow.
pub(crate) const HEADER_LEN: usize = 27;

/// The capture pattern every page starts with.
const MAGIC: &[u8; 4] = b"OggS";

/// The one version of the framing RFC 3533 defines, and the only one a TAF holds.
const VERSION: u8 = 0;

/// How far into the header the checksum sits.
const CHECKSUM_AT: usize = 22;

/// The bytes the checksum occupies.
const CHECKSUM_LEN: usize = 4;

/// The type flag that marks the first page of a stream.
const FLAG_FIRST: u8 = 0x02;

/// The type flag that marks the last page of a stream.
const FLAG_LAST: u8 = 0x04;

/// The lacing value that says its packet carries on into the next segment.
const CONTINUES: u8 = 255;
