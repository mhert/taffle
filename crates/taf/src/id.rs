//! The identifiers a TAF file carries: its audio id and its chapter starts.

use core::fmt;

/// The audio id of a TAF file: the header's `audio_id` field, and the serial number of every
/// Ogg page in the file.
///
/// teddycloud derives what it writes here from the clock — `time(NULL) - 0x50000000`, a Unix
/// timestamp with a fixed offset taken off it — but nothing in the format depends on that, so
/// every `u32` is a valid audio id and construction cannot fail. Fallibility belongs where ids
/// are read out of files, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioId(u32);

impl AudioId {
    /// Wraps a raw audio id.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw audio id.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for AudioId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Where a chapter starts, counted in 4096-byte blocks of the audio region.
///
/// These are block indices, not Ogg page sequence numbers: block `n` is Ogg page `n + 2` and
/// begins at file offset `4096 * (n + 1)`, except block 0, whose audio begins at 4608 because
/// the two Opus header pages share that first block. Upstream teddycloud calls the header field
/// `track_page_nums`, which is a misnomer; `FORMAT.md` in this crate documents the layout and is
/// authoritative.
///
/// Every `u32` is a structurally valid index, so construction cannot fail; whether an index
/// falls inside a given file is a question for whoever reads that file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockIndex(u32);

impl BlockIndex {
    /// Wraps a raw block index.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw block index.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn audio_id_round_trips_and_displays_as_decimal() {
        let id = AudioId::new(0x5017_1234);

        assert_eq!(id.get(), 0x5017_1234);
        assert_eq!(alloc::format!("{id}"), "1343689268");
    }

    #[test]
    fn block_index_round_trips() {
        let index = BlockIndex::new(0x5017_1234);

        assert_eq!(index.get(), 0x5017_1234);
    }
}
