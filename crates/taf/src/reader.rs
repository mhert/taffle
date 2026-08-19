//! Reading a TAF's audio region as it goes past, and checking that it is one.
//!
//! [`Validator`] is handed a file's parsed header block and then the audio region behind it — one
//! 4096-byte block at a time, in the order it lies in the file. The block is the unit because it
//! is what a box reads a TAF in, and it is what keeps the walk alloc-free: nothing is buffered
//! between blocks, so a file of any length is validated out of the one block the caller happens to
//! be holding. Hashing the bytes on the way is optional, and where the digest comes from is the
//! caller's business — [`finish`](Validator::finish) takes the hash the caller finalized rather
//! than finalizing one itself.
//!
//! What a block holds is fixed. The first one carries the two pages the Opus stream opens with —
//! which span exactly the first 512 bytes of it — and then the audio page that closes the block;
//! every block behind it carries exactly one page of exactly a block. That is what lets a box seek
//! to a chapter by multiplying: `4096 + 4096 * block`, plus `0x200` for the first chapter, to skip
//! the two pages sharing its block. Both offsets are checked here before anything else, since a
//! file that misses either has chapters that cannot be sought to. `FORMAT.md` in this crate
//! describes the layout and is authoritative.
//!
//! An audio region that is not a whole number of blocks is therefore not one this reads at all:
//! the short last read is refused as not being a block, and the file comes up short when the walk
//! is finished. A file with no audio in it is exactly that case — its whole region is the 512
//! bytes of the two pages the stream opens with, an eighth of a block — and both teddycloud's
//! writer and this crate's leave one behind when they are asked to close a file nothing was
//! written to.
//!
//! Two things a validator of plain Ogg would do are deliberately not done here. It does not look
//! for a page that ends the stream — a TAF has none, teddycloud stops writing at a block boundary
//! and never flushes what it still holds — and it does not join packets across pages, because a
//! TAF never splits one: a page that states the continued-packet flag is refused rather than
//! pieced together, since the packet it continues would span two blocks and a box seeking to a
//! chapter block would begin mid-packet.

use core::fmt;

use crate::digest::Sha1Update;
use crate::header::{ChapterPages, HeaderView, BLOCK_LEN};
use crate::id::{AudioId, BlockIndex};
use crate::ogg::{PageError, PageView, OPUS_HEAD_MAGIC, OPUS_TAGS_MAGIC};

/// The bytes one block occupies, counted the way an audio region's length counts them.
///
/// The same 4096 as [`BLOCK_LEN`], in the width a file's length is stated and summed in.
const BLOCK_BYTES: u64 = 4096;

/// The pages every block of a TAF holds, but for the first.
const BLOCK_PAGES: usize = 1;

/// The pages the Opus stream opens with, which share the front of the first block with the audio
/// page behind them.
const HEADER_PAGES: usize = 2;

/// The bytes those two pages span, and so where the first block's audio page begins.
///
/// This is the one page offset in a TAF that is not a multiple of the block size, and a box
/// depends on it: it seeks a chapter to `4096 + 4096 * block` and adds `0x200` for the first
/// chapter, to skip the two pages sharing block 0. A file whose audio page starts anywhere else
/// is one whose first chapter cannot be sought to, however well the rest of it adds up.
/// teddycloud lands on it by fixing the `OpusTags` packet at 436 bytes: 47 + 465 = 512.
const HEADER_PAGES_LEN: usize = 512;

/// The pages the first block of a TAF holds: the two the Opus stream opens with, and the audio
/// page that closes the block.
const FIRST_BLOCK_PAGES: usize = HEADER_PAGES + BLOCK_PAGES;

/// Why a TAF's audio region is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidateError {
    /// A page could not be read at all; carries what the page reader said about it.
    ///
    /// [`PageError::TruncatedBody`] and [`PageError::TooShort`] are also what a page reaching past
    /// the end of its block comes to: a block is handed to the page reader on its own, and a TAF's
    /// pages never cross a block boundary.
    Page(PageError),
    /// The bytes pushed were not one whole block; carries how many there were.
    WrongBlockLen {
        /// The bytes that were pushed.
        len: usize,
    },
    /// A page does not end where the format puts its end: the last page of a block ends exactly
    /// where the block does, the two pages the Opus stream opens with end exactly 512 bytes into
    /// the first block, and no other page of a block ends at either.
    Misaligned {
        /// The sequence number the page states, which is also where it sits in the file.
        page: u32,
    },
    /// A page states a serial number other than the audio id the header states.
    SerialMismatch,
    /// A page's sequence number does not follow the page before it; carries the one it should
    /// have stated.
    SequenceGap {
        /// The sequence number the page had to state.
        expected: u32,
    },
    /// A page's granule position is behind the page before it, so the file plays backwards.
    GranuleRegression {
        /// The sequence number the page states.
        page: u32,
    },
    /// The Opus stream never begins: the file's first page does not state the BOS flag, or the
    /// file holds no page at all.
    MissingBos,
    /// A page other than the first states the BOS flag.
    UnexpectedBos {
        /// The sequence number the page states.
        page: u32,
    },
    /// A page states the EOS flag, which no page of a TAF does.
    UnexpectedEos {
        /// The sequence number the page states.
        page: u32,
    },
    /// A page states the continued-packet flag, which no page of a TAF does.
    ContinuedPacket {
        /// The sequence number the page states.
        page: u32,
    },
    /// One of the two pages a TAF opens with does not carry the Opus header packet it must:
    /// `OpusHead` on page 0, `OpusTags` on page 1.
    MissingOpusHeader {
        /// The sequence number the page states.
        page: u32,
    },
    /// The audio region is not as long as the header states.
    LengthMismatch {
        /// The length the header states.
        header: u32,
        /// The bytes that were pushed.
        actual: u64,
    },
    /// The audio region does not hash to what the header states.
    Sha1Mismatch,
    /// The header starts a chapter at a block the walk never reached; carries the block index.
    ///
    /// Chapters are matched against the blocks in the order the header lists them, so this also
    /// covers a list that repeats a block or goes backwards: an entry the walk passed without
    /// matching is one the file does not hold.
    ChapterPageMissing(u32),
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Page(error) => fmt::Display::fmt(&error, f),
            Self::WrongBlockLen { len } => write!(
                f,
                "a TAF's audio region is read one {BLOCK_LEN}-byte block at a time, and {len} bytes are not one"
            ),
            Self::Misaligned { page } => {
                write!(f, "Ogg page {page} does not end where the TAF block it lies in does")
            }
            Self::SerialMismatch => {
                f.write_str("an Ogg page states a serial number other than the file's audio id")
            }
            Self::SequenceGap { expected } => {
                write!(f, "an Ogg page states a sequence number other than {expected}")
            }
            Self::GranuleRegression { page } => write!(
                f,
                "Ogg page {page} states a granule position behind the page before it"
            ),
            Self::MissingBos => f.write_str("a TAF's Opus stream never begins"),
            Self::UnexpectedBos { page } => {
                write!(f, "Ogg page {page} begins a stream that page 0 already began")
            }
            Self::UnexpectedEos { page } => write!(
                f,
                "Ogg page {page} ends the stream, which no page of a TAF does"
            ),
            Self::ContinuedPacket { page } => write!(
                f,
                "Ogg page {page} carries on a packet from the page before it, which no page of a TAF does"
            ),
            Self::MissingOpusHeader { page } => write!(
                f,
                "Ogg page {page} does not carry the Opus header packet it opens the stream with"
            ),
            Self::LengthMismatch { header, actual } => write!(
                f,
                "a TAF's audio region is {actual} bytes, and its header states {header}"
            ),
            Self::Sha1Mismatch => {
                f.write_str("a TAF's audio region does not hash to what its header states")
            }
            Self::ChapterPageMissing(block) => write!(
                f,
                "a TAF's header starts a chapter at block {block}, which the file does not hold"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ValidateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Page(error) => Some(error),
            _ => None,
        }
    }
}

/// Where a chapter starts, and how much audio the file carries before it.
///
/// A validator hands one of these over as the block a chapter starts at goes past, which is what
/// lets a caller build a chapter table while the file streams by without holding a list of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterInfo {
    /// The block the chapter starts at, as the header states it.
    pub block: BlockIndex,
    /// The granule position the chapter's audio begins at: the samples of one channel that the
    /// pages before its block carry, raw, with the Opus pre-skip counted in the way
    /// [`Summary::total_samples`] counts it. Seconds into the audio means taking that pre-skip off
    /// first — except at a file's first chapter, whose granule is 0 because no page comes before
    /// it and whose audio is where the file starts.
    pub granule: u64,
}

/// What a validated audio region came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// The Ogg pages the region holds.
    pub pages: u32,
    /// The granule position the region's last page states, raw: the samples of one channel the
    /// file carries, the Opus pre-skip counted in with them.
    ///
    /// A player takes that pre-skip off before it counts seconds — [`OPUS_PRE_SKIP`] samples in
    /// every TAF — so the audio runs `(total_samples - 312) / 48_000` seconds at the 48 kHz a TAF
    /// is encoded at. And it is what the file *carries*, which is up to a block less than what
    /// teddycloud handed its encoder, since teddycloud drops whatever it still has buffered when
    /// it closes a file.
    ///
    /// [`OPUS_PRE_SKIP`]: crate::ogg::OPUS_PRE_SKIP
    pub total_samples: u64,
    /// The bytes the region occupies, which the header states and the walk found it to have.
    pub audio_bytes: u32,
    /// The chapters the walk met, which is every chapter the header lists.
    pub chapters_seen: u32,
}

/// A TAF's audio region, checked block by block as it goes past.
///
/// [`push_block`](Validator::push_block) takes the blocks in file order and
/// [`finish`](Validator::finish) says whether they added up to the file the header describes.
/// Every page is checked as it goes by, in this order: the serial number it states, its sequence
/// number, the BOS, EOS and continued-packet flags, its granule position, the Opus header packet
/// the first two pages carry, and last where the page ends inside its block. Checking the sequence
/// numbers first is what lets everything behind it speak of *page n*: a page that passed that
/// check is the nth page of the file.
///
/// A push takes the block whole or not at all. A block that is refused is not hashed and does not
/// count towards the file, so a caller that pushed the wrong bytes can push the right ones and
/// carry on — and a caller that stops at the first error leaves a validator that reports the file
/// as short, which it is.
///
/// # Examples
///
/// ```
/// use taf::header::{HeaderView, BLOCK_LEN};
/// use taf::reader::Validator;
///
/// # let file = include_bytes!("../tests/fixtures/golden-sine.taf");
/// // A TAF file: the header block, and behind it the audio region in blocks of its own.
/// let header = HeaderView::parse(&file[..BLOCK_LEN])?;
/// let mut validator = Validator::new(&header);
/// let mut chapters = Vec::new();
///
/// for block in file[BLOCK_LEN..].chunks(BLOCK_LEN) {
///     if let Some(chapter) = validator.push_block(block, None)? {
///         chapters.push(chapter.block.get());
///     }
/// }
///
/// // Finishing without a hash checks the file's structure and leaves its SHA-1 alone; a caller
/// // that fed a digest along the way hands the hash it finalized in here instead.
/// let summary = validator.finish(None)?;
///
/// assert_eq!(summary.pages, 29);
/// assert_eq!(summary.audio_bytes, 110_592);
/// assert_eq!(chapters, [0]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct Validator<'h> {
    /// What the header states the audio region hashes to.
    sha1: &'h [u8; 20],
    /// What it states the region's length in bytes is.
    data_length: u32,
    /// The id every page of the file states as its serial number.
    audio_id: AudioId,
    /// The chapter starts still to be met, in the order the header lists them.
    chapters: ChapterPages<'h>,
    /// The blocks taken so far, which is the index of the block a push is about.
    blocks: u32,
    /// The pages they held, which is the sequence number the next page states.
    pages: u32,
    /// The granule position the last of those pages states.
    granule: u64,
    /// The chapters met so far.
    chapters_seen: u32,
}

impl<'h> Validator<'h> {
    /// Starts validating the audio region of the file `header` describes.
    #[must_use]
    pub fn new(header: &'h HeaderView<'h>) -> Self {
        Self {
            sha1: header.sha1(),
            data_length: header.data_length(),
            audio_id: header.audio_id(),
            chapters: header.chapter_pages(),
            blocks: 0,
            pages: 0,
            granule: 0,
            chapters_seen: 0,
        }
    }

    /// Takes the next block of the audio region, hashing it into `digest` if there is one.
    ///
    /// `block` is exactly [`BLOCK_LEN`] bytes of the file, starting at offset `4096 * (n + 1)` for
    /// the nth call — the header block is not part of this and is never pushed. The digest, if the
    /// caller keeps one, is fed the block whole: a TAF's hash covers the audio region from the
    /// first of its bytes, the two pages the Opus stream opens with included.
    ///
    /// Returns the chapter this block starts, if it starts one, with the granule position its
    /// audio begins at — which is a caller's chance to build a chapter table as the file goes by.
    ///
    /// # Errors
    ///
    /// - [`ValidateError::WrongBlockLen`] if `block` is not exactly one block. Nothing is hashed
    ///   and nothing is counted, which is what a short last read of a file comes to.
    /// - [`ValidateError::Page`] if a page of the block could not be read, which includes a page
    ///   that reaches past the block's end.
    /// - [`ValidateError::SerialMismatch`], [`ValidateError::SequenceGap`],
    ///   [`ValidateError::MissingBos`], [`ValidateError::UnexpectedBos`],
    ///   [`ValidateError::UnexpectedEos`], [`ValidateError::ContinuedPacket`],
    ///   [`ValidateError::GranuleRegression`] or [`ValidateError::MissingOpusHeader`] if a page
    ///   states something no page of a TAF states.
    /// - [`ValidateError::Misaligned`] if a page does not end where its block requires: the end of
    ///   the block, or the 512 bytes the two pages the stream opens with span.
    pub fn push_block(
        &mut self,
        block: &[u8],
        digest: Option<&mut dyn Sha1Update>,
    ) -> Result<Option<ChapterInfo>, ValidateError> {
        let block: &[u8; BLOCK_LEN] = block
            .try_into()
            .map_err(|_| ValidateError::WrongBlockLen { len: block.len() })?;
        let (pages, granule) = self.walk(block)?;

        if let Some(digest) = digest {
            digest.update(block);
        }

        // A chapter starts where its block starts, so the audio before it is what the pages before
        // this block carried — which is what the granule position still says here.
        let chapter = self.take_chapter();

        // A TAF is half a million blocks at the very most, so this counts every one of them;
        // saturating rather than wrapping keeps an impossible file from passing for a plausible
        // one, and a walk that did saturate fails the length check anyway.
        self.blocks = self.blocks.saturating_add(1);
        self.pages = pages;
        self.granule = granule;

        Ok(chapter)
    }

    /// Says whether the blocks pushed so far are the audio region the header describes.
    ///
    /// `digest_result` is the hash of everything pushed, finalized by whoever owns the digest.
    /// Handing over `None` validates the file's structure and leaves its hash unchecked, which is
    /// what a caller that pushed no digest either does.
    ///
    /// # Errors
    ///
    /// - [`ValidateError::LengthMismatch`] if the blocks taken do not come to the length the
    ///   header states.
    /// - [`ValidateError::MissingBos`] if no block was ever taken, so the stream never began.
    /// - [`ValidateError::ChapterPageMissing`] if the header starts a chapter at a block the walk
    ///   never met.
    /// - [`ValidateError::Sha1Mismatch`] if `digest_result` is not the hash the header states.
    ///   This is the last thing checked, so a file that is broken in some other way says so
    ///   rather than only saying that its bytes do not add up.
    pub fn finish(mut self, digest_result: Option<[u8; 20]>) -> Result<Summary, ValidateError> {
        // A block index is a `u32` and a block is 4096 bytes, so this stays far inside a `u64`.
        let actual = u64::from(self.blocks) * BLOCK_BYTES;

        if actual != u64::from(self.data_length) {
            return Err(ValidateError::LengthMismatch {
                header: self.data_length,
                actual,
            });
        }
        if self.pages == 0 {
            return Err(ValidateError::MissingBos);
        }
        if let Some(missing) = self.chapters.next() {
            return Err(ValidateError::ChapterPageMissing(missing.get()));
        }
        if let Some(hash) = digest_result {
            if &hash != self.sha1 {
                return Err(ValidateError::Sha1Mismatch);
            }
        }

        Ok(Summary {
            pages: self.pages,
            total_samples: self.granule,
            // The length the header states, which the walk has just been found to come to.
            audio_bytes: self.data_length,
            chapters_seen: self.chapters_seen,
        })
    }

    /// Reads the pages of one block, and returns where they leave the walk: the pages the file has
    /// behind this block, and the granule position its last page states.
    fn walk(&self, block: &[u8; BLOCK_LEN]) -> Result<(u32, u64), ValidateError> {
        let held = if self.blocks == 0 {
            FIRST_BLOCK_PAGES
        } else {
            BLOCK_PAGES
        };
        let mut at = 0;
        let mut pages = self.pages;
        let mut granule = self.granule;

        for index in 0..held {
            let view = PageView::parse(block.get(at..).unwrap_or_default())
                .map_err(ValidateError::Page)?;

            self.check(&view, pages, granule)?;

            // The page was read out of what is left of the block, so it ends inside the block.
            at += view.total_len();

            // And where inside it is fixed twice over. The last page of a block ends exactly where
            // the block does, and the two pages the stream opens with end exactly
            // [`HEADER_PAGES_LEN`] bytes into the first block — the offset a box adds to a
            // chapter's block to skip them. No other page of a block ends at either.
            let closes_the_block = index + 1 == held;
            let closes_the_header_pages = held == FIRST_BLOCK_PAGES && index + 1 == HEADER_PAGES;

            if closes_the_block != (at == BLOCK_LEN)
                || closes_the_header_pages != (at == HEADER_PAGES_LEN)
            {
                return Err(ValidateError::Misaligned { page: pages });
            }

            granule = view.granule_position();
            // The sequence numbers a page states are a `u32` and this counts with them, so a
            // stream long enough to saturate this is one that cannot state its own page numbers.
            pages = pages.saturating_add(1);
        }

        Ok((pages, granule))
    }

    /// Checks one page against what a TAF states about the `page`th page of a file whose pages so
    /// far reached `granule`.
    fn check(&self, view: &PageView<'_>, page: u32, granule: u64) -> Result<(), ValidateError> {
        if view.serial() != self.audio_id.get() {
            return Err(ValidateError::SerialMismatch);
        }
        if view.sequence() != page {
            return Err(ValidateError::SequenceGap { expected: page });
        }
        if view.is_first() != (page == 0) {
            return Err(if page == 0 {
                ValidateError::MissingBos
            } else {
                ValidateError::UnexpectedBos { page }
            });
        }
        if view.is_last() {
            return Err(ValidateError::UnexpectedEos { page });
        }
        if view.is_continued() {
            return Err(ValidateError::ContinuedPacket { page });
        }
        if view.granule_position() < granule {
            return Err(ValidateError::GranuleRegression { page });
        }

        // The two packets the Opus stream opens with, which are what tell a TAF from any other Ogg
        // stream that happens to be laid out in blocks. Everything behind them is audio this crate
        // does not decode, so nothing else is read out of a packet.
        let opens_with = match page {
            0 => OPUS_HEAD_MAGIC,
            1 => OPUS_TAGS_MAGIC,
            _ => return Ok(()),
        };

        if view
            .packets()
            .next()
            .is_some_and(|packet| packet.starts_with(opens_with))
        {
            Ok(())
        } else {
            Err(ValidateError::MissingOpusHeader { page })
        }
    }

    /// Takes the chapter starting at the block being pushed, if the header states one there.
    ///
    /// The header's chapter list is matched in the order it states, one entry per block at most:
    /// an entry that this walk goes past unmatched — one that repeats a block, goes backwards, or
    /// lies past the end of the file — is what [`finish`](Validator::finish) reports as missing.
    fn take_chapter(&mut self) -> Option<ChapterInfo> {
        let mut rest = self.chapters.clone();
        let start = rest.next().filter(|start| start.get() == self.blocks)?;

        self.chapters = rest;
        // A block index is a `u32`, so a file cannot hold more chapters than this counts.
        self.chapters_seen = self.chapters_seen.saturating_add(1);

        Some(ChapterInfo {
            block: start,
            granule: self.granule,
        })
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
    use crate::digest::Sha1;
    use crate::header::encode_header;
    use crate::ogg::{crc32, opus_head, opus_tags, PageBuilder, OPUS_PRE_SKIP};
    use crate::writer::{TafWriter, Tags};
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    const GOLDEN: &[u8] = include_bytes!("../tests/fixtures/golden-sine.taf");

    /// Where a TAF's audio region starts: behind the header block.
    const AUDIO_AT: usize = BLOCK_LEN;

    /// The golden file's audio id, which every one of its pages states as its serial number.
    const AUDIO_ID: AudioId = AudioId::new(444_913_029);

    /// The samples of one channel an Opus packet of a TAF carries: 60 ms at 48 kHz.
    const SAMPLES: u32 = 2880;

    /// Where RFC 3533 puts a page's type flags.
    const FLAGS_AT: usize = 5;

    /// Where it puts the count of the lacing values that follow the page header.
    const SEGMENTS_AT: usize = 26;

    /// Where it puts the checksum, and the bytes it occupies.
    const CHECKSUM_AT: usize = 22;
    const CHECKSUM_LEN: usize = 4;

    /// The type flag that says the page's first packet carries on one the page before it began.
    const CONTINUED: u8 = 0x01;

    /// The type flag that marks the first page of a stream.
    const BOS: u8 = 0x02;

    /// The type flag that marks the last page of a stream.
    const EOS: u8 = 0x04;

    /// The packet that fills the first audio page: 3543 bytes occupy the 3557 it has for them.
    const FIRST_AUDIO_PACKET: usize = 3543;

    /// The packet that fills a block-aligned page: 4053 bytes occupy the 4069 one has.
    const PAGE_PACKET: usize = 4053;

    /// The pages a writer emitted, which a test reads once the writer is done with them.
    type Pages = RefCell<Vec<Vec<u8>>>;

    struct RustCrypto(sha1::Sha1);

    impl Sha1 for RustCrypto {
        fn update(&mut self, data: &[u8]) {
            sha1::Digest::update(&mut self.0, data);
        }

        fn finalize(self) -> [u8; 20] {
            sha1::Digest::finalize(self.0).into()
        }
    }

    fn digest() -> RustCrypto {
        RustCrypto(<sha1::Sha1 as sha1::Digest>::new())
    }

    fn sha1_of(bytes: &[u8]) -> [u8; 20] {
        let mut digest = digest();
        Sha1::update(&mut digest, bytes);

        digest.finalize()
    }

    /// What validating an audio region came to.
    #[derive(Debug)]
    struct Run {
        /// The chapters the blocks that were taken reported, in the order they reported them.
        chapters: Vec<ChapterInfo>,
        /// The block the validator refused, if it refused one, and what it said about it.
        refused: Option<(usize, ValidateError)>,
        /// What `finish` made of the file.
        summary: Result<Summary, ValidateError>,
    }

    /// Validates an audio region the way a box reads one: block by block, hashing as it goes, and
    /// stopping at the first block the validator refuses.
    fn validate(header: &HeaderView<'_>, audio: &[u8]) -> Run {
        run(header, audio, true)
    }

    /// The same walk with no digest anywhere: neither the blocks nor `finish` are hashed.
    fn validate_unhashed(header: &HeaderView<'_>, audio: &[u8]) -> Run {
        run(header, audio, false)
    }

    fn run(header: &HeaderView<'_>, audio: &[u8], hashed: bool) -> Run {
        let mut validator = Validator::new(header);
        let mut digest = digest();
        let mut chapters = Vec::new();
        let mut refused = None;

        for (at, block) in audio.chunks(BLOCK_LEN).enumerate() {
            let pushed = if hashed {
                validator.push_block(block, Some(&mut digest))
            } else {
                validator.push_block(block, None)
            };

            match pushed {
                Ok(chapter) => chapters.extend(chapter),
                Err(error) => {
                    refused = Some((at, error));
                    break;
                }
            }
        }

        let summary = validator.finish(hashed.then(|| digest.finalize()));

        Run {
            chapters,
            refused,
            summary,
        }
    }

    /// A packet of `len` bytes that is not audio at all: nothing in a reader looks inside a packet
    /// beyond the magic the two the stream opens with state.
    fn junk(len: usize) -> Vec<u8> {
        vec![0xa5; len]
    }

    /// A page of the file's stream, carrying `packets` and stating `flags`.
    ///
    /// Built with the crate's own [`PageBuilder`] and then restated with the flags the test asked
    /// for — the builder has a word for BOS and EOS and none for the continued-packet flag, which
    /// no TAF states and a reader still has to answer for. The checksum is summed again over the
    /// bytes that come out, so every page here is one a reader accepts unless a test made it
    /// otherwise.
    fn page(sequence: u32, granule: u64, flags: u8, packets: &[&[u8]]) -> Vec<u8> {
        let mut builder = PageBuilder::new(AUDIO_ID.get(), sequence);
        builder.granule_position(granule);
        for packet in packets {
            builder.push_packet(packet).unwrap();
        }

        let mut page = builder.finish();
        page[FLAGS_AT] = flags;
        reseal(&mut page);

        page
    }

    /// Sums a page again after a test changed it, so that only what the test meant to break is
    /// broken.
    fn reseal(page: &mut [u8]) {
        page[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_LEN].fill(0);
        let checksum = crc32(page).to_le_bytes();
        page[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_LEN].copy_from_slice(&checksum);
    }

    /// The page a TAF opens with: the `OpusHead` packet, stated with whatever flags a test needs.
    fn head_page(flags: u8) -> Vec<u8> {
        page(0, 0, flags, &[&opus_head(OPUS_PRE_SKIP)])
    }

    /// The page behind it, carrying the `OpusTags` packet.
    fn tags_page() -> Vec<u8> {
        page(1, 0, 0, &[&opus_tags("taffle", &[]).unwrap()])
    }

    /// The first block of a TAF: the two pages the Opus stream opens with, and the audio page that
    /// closes the block.
    fn first_block() -> Vec<u8> {
        let mut block = head_page(BOS);
        block.extend(tags_page());
        block.extend(page(2, u64::from(SAMPLES), 0, &[&junk(FIRST_AUDIO_PACKET)]));

        block
    }

    /// A block of the audio region behind it: one page, exactly a block long.
    ///
    /// Block `n` is page `n + 2`, and every block carries one packet, so the granule position runs
    /// with the block.
    fn audio_block(block: u32, flags: u8) -> Vec<u8> {
        page(
            block + 2,
            u64::from(SAMPLES) * u64::from(block + 1),
            flags,
            &[&junk(PAGE_PACKET)],
        )
    }

    /// An audio region of `blocks` blocks behind the first one.
    fn audio(blocks: u32) -> Vec<u8> {
        let mut audio = first_block();

        for block in 1..=blocks {
            audio.extend(audio_block(block, 0));
        }

        audio
    }

    /// The header block a file of this audio region and these chapters has.
    ///
    /// The length and the hash it states are the ones the region really has, so a test that does
    /// not break one of them is testing something else.
    fn header_block(audio: &[u8], chapters: &[u32]) -> [u8; BLOCK_LEN] {
        encode_header(
            &sha1_of(audio),
            u32::try_from(audio.len()).unwrap(),
            AUDIO_ID,
            chapters,
        )
        .unwrap()
    }

    /// What a file of `audio` states about itself, with one chapter at block 0 as every TAF has.
    fn header_of(audio: &[u8]) -> [u8; BLOCK_LEN] {
        header_block(audio, &[0])
    }

    /// The audio packets of the golden file: everything its pages from sequence 2 on carry.
    fn golden_packets() -> Vec<&'static [u8]> {
        let mut packets = Vec::new();
        let mut at = AUDIO_AT;

        while at < GOLDEN.len() {
            let view = PageView::parse(&GOLDEN[at..]).unwrap();

            if view.sequence() >= 2 {
                packets.extend(view.packets().take(256));
            }
            at += view.total_len();
        }

        assert_eq!(packets.len(), 161);

        packets
    }

    /// The `OpusTags` content a file written here carries, which nothing about validating reads.
    fn tags() -> Tags<'static> {
        Tags::new("taffle", &[])
    }

    /// The summary the golden file validates to.
    fn golden_summary() -> Summary {
        Summary {
            pages: 29,
            // The last page's granule, not the 478 080 the header's own bookkeeping states:
            // teddycloud drops the packets it still has buffered when it closes a file.
            total_samples: 463_680,
            audio_bytes: 110_592,
            chapters_seen: 1,
        }
    }

    #[test]
    fn validates_the_golden_files_audio_region_block_by_block() {
        let header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();
        let run = validate(&header, &GOLDEN[AUDIO_AT..]);

        assert_eq!(run.refused, None);
        assert_eq!(run.summary, Ok(golden_summary()));
        // 9.66 seconds of audio at 48 kHz, and the one chapter every TAF starts at block 0.
        assert_eq!(golden_summary().total_samples / 48_000, 9);
        assert_eq!(
            run.chapters,
            [ChapterInfo {
                block: BlockIndex::new(0),
                granule: 0
            }]
        );
    }

    #[test]
    fn validates_the_golden_file_without_a_digest_at_all() {
        let header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();
        let run = validate_unhashed(&header, &GOLDEN[AUDIO_AT..]);

        assert_eq!(run.refused, None);
        assert_eq!(run.summary, Ok(golden_summary()));
    }

    #[test]
    fn reports_an_audio_region_that_does_not_hash_to_what_the_header_states() {
        let header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();
        let mut audio = GOLDEN[AUDIO_AT..].to_vec();

        // One page of the file, restated with a byte of its audio changed and summed again: the
        // framing is as sound as it was, and the region is as long as it was, so the hash is the
        // one thing left that says the file was tampered with.
        let at = 2 * BLOCK_LEN;
        let view = PageView::parse(&audio[at..]).unwrap();
        let mut packets: Vec<Vec<u8>> = view.packets().take(256).map(<[u8]>::to_vec).collect();
        packets[0][10] ^= 0x01;
        let restated = page(
            view.sequence(),
            view.granule_position(),
            0,
            &packets.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        );

        assert_eq!(restated.len(), BLOCK_LEN);
        audio[at..at + BLOCK_LEN].copy_from_slice(&restated);

        let hashed = validate(&header, &audio);
        let unhashed = validate_unhashed(&header, &audio);

        assert_eq!(hashed.refused, None);
        assert_eq!(hashed.summary, Err(ValidateError::Sha1Mismatch));
        // Everything but the hash still adds up, so a walk that does not hash accepts it.
        assert_eq!(unhashed.refused, None);
        assert_eq!(unhashed.summary, Ok(golden_summary()));
    }

    #[test]
    fn reports_the_page_whose_bytes_do_not_add_up() {
        let header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();
        let mut audio = GOLDEN[AUDIO_AT..].to_vec();
        audio[4 * BLOCK_LEN + 100] ^= 0x01;

        let run = validate(&header, &audio);

        assert_eq!(
            run.refused,
            Some((4, ValidateError::Page(PageError::BadCrc)))
        );
        // The block that was refused is not one of the file's, so the walk is four blocks long.
        assert_eq!(
            run.summary,
            Err(ValidateError::LengthMismatch {
                header: 110_592,
                actual: 4 * 4096
            })
        );
    }

    #[test]
    fn reports_an_audio_region_that_stops_short() {
        let header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();
        let short = &GOLDEN[AUDIO_AT..GOLDEN.len() - 2048];
        let dropped = &GOLDEN[AUDIO_AT..GOLDEN.len() - BLOCK_LEN];

        // The last page cut in half is not a block, so it is not pushed at all — and what was
        // pushed is a block short of what the header states.
        let cut = validate(&header, short);

        assert_eq!(
            cut.refused,
            Some((26, ValidateError::WrongBlockLen { len: 2048 }))
        );
        assert_eq!(
            cut.summary,
            Err(ValidateError::LengthMismatch {
                header: 110_592,
                actual: 106_496
            })
        );

        // With that page gone altogether every push is taken, and the file is simply short.
        let ended = validate(&header, dropped);

        assert_eq!(ended.refused, None);
        assert_eq!(
            ended.summary,
            Err(ValidateError::LengthMismatch {
                header: 110_592,
                actual: 106_496
            })
        );
    }

    #[test]
    fn validates_a_file_the_writer_wrote_from_scratch() {
        let pages = Pages::default();
        let mut writer = TafWriter::new(digest(), AUDIO_ID, tags(), |page: &[u8]| {
            pages.borrow_mut().push(page.to_vec());
        })
        .unwrap();

        for (at, packet) in golden_packets().iter().enumerate() {
            if at == 40 || at == 100 {
                writer.begin_chapter().unwrap();
            }
            writer.add_packet(packet, SAMPLES).unwrap();
        }

        let block = writer.finalize().unwrap();
        let header = HeaderView::parse(&block).unwrap();
        let audio = pages.borrow().concat();
        let starts: Vec<u32> = header.chapter_pages().map(BlockIndex::get).collect();
        let run = validate(&header, &audio);

        assert_eq!(run.refused, None);
        assert_eq!(
            run.summary,
            Ok(Summary {
                pages: u32::try_from(pages.borrow().len()).unwrap(),
                // Every packet handed in is in the file: this writer pads its last page rather
                // than dropping what it holds.
                total_samples: 161 * u64::from(SAMPLES),
                audio_bytes: header.data_length(),
                chapters_seen: 3,
            })
        );

        // A chapter starts where its block starts, so its granule is the audio before it: the
        // packets the writer took before the chapter was begun, 2880 samples apiece.
        assert_eq!(starts.len(), 3);
        assert_eq!(
            run.chapters,
            [
                ChapterInfo {
                    block: BlockIndex::new(starts[0]),
                    granule: 0
                },
                ChapterInfo {
                    block: BlockIndex::new(starts[1]),
                    granule: 40 * u64::from(SAMPLES)
                },
                ChapterInfo {
                    block: BlockIndex::new(starts[2]),
                    granule: 100 * u64::from(SAMPLES)
                },
            ]
        );
    }

    #[test]
    fn validates_a_file_built_out_of_pages_of_its_own() {
        let audio = audio(2);
        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();
        let run = validate(&header, &audio);

        assert_eq!(audio.len(), 3 * BLOCK_LEN);
        assert_eq!(run.refused, None);
        assert_eq!(
            run.summary,
            Ok(Summary {
                pages: 5,
                total_samples: 3 * u64::from(SAMPLES),
                audio_bytes: 3 * 4096,
                chapters_seen: 1,
            })
        );
    }

    #[test]
    fn takes_the_audio_region_one_whole_block_at_a_time() {
        let audio = audio(1);
        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        for len in [0, BLOCK_LEN - 1, BLOCK_LEN + 1, 2 * BLOCK_LEN] {
            let mut validator = Validator::new(&header);

            assert_eq!(
                validator.push_block(&junk(len), None),
                Err(ValidateError::WrongBlockLen { len }),
                "{len} bytes"
            );
        }
    }

    #[test]
    fn takes_a_block_whole_or_leaves_the_walk_where_it_was() {
        let audio = audio(1);
        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();
        let mut validator = Validator::new(&header);
        let mut digest = digest();

        // The file's second block, pushed where its first one belongs: a block the validator
        // refuses is not hashed and does not count, so the walk carries on from where it was.
        assert_eq!(
            validator.push_block(&audio[BLOCK_LEN..], Some(&mut digest)),
            Err(ValidateError::SequenceGap { expected: 0 })
        );

        for block in audio.chunks(BLOCK_LEN) {
            assert!(validator.push_block(block, Some(&mut digest)).is_ok());
        }

        assert_eq!(
            validator.finish(Some(digest.finalize())),
            Ok(Summary {
                pages: 4,
                total_samples: 2 * u64::from(SAMPLES),
                audio_bytes: 2 * 4096,
                chapters_seen: 1,
            })
        );
    }

    #[test]
    fn reports_a_first_page_that_does_not_begin_the_stream() {
        let mut audio = head_page(0);
        audio.extend(tags_page());
        audio.extend(page(2, u64::from(SAMPLES), 0, &[&junk(FIRST_AUDIO_PACKET)]));

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((0, ValidateError::MissingBos))
        );
    }

    #[test]
    fn reports_a_later_page_that_begins_the_stream_again() {
        let mut audio = first_block();
        audio.extend(audio_block(1, BOS));

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((1, ValidateError::UnexpectedBos { page: 3 }))
        );
    }

    #[test]
    fn reports_a_page_that_ends_the_stream() {
        let mut audio = first_block();
        audio.extend(audio_block(1, EOS));

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((1, ValidateError::UnexpectedEos { page: 3 }))
        );
    }

    #[test]
    fn reports_a_page_that_carries_a_packet_on_from_the_page_before_it() {
        let mut audio = first_block();
        audio.extend(audio_block(1, CONTINUED));

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((1, ValidateError::ContinuedPacket { page: 3 }))
        );
    }

    #[test]
    fn reports_pages_that_do_not_carry_the_packets_an_opus_stream_opens_with() {
        // A first page carrying anything but `OpusHead`, and a second carrying anything but
        // `OpusTags`: this is what tells a TAF from any other Ogg stream that happens to be laid
        // out in blocks.
        let mut without_head = page(0, 0, BOS, &[&junk(19)]);
        without_head.extend(tags_page());
        without_head.extend(page(2, u64::from(SAMPLES), 0, &[&junk(FIRST_AUDIO_PACKET)]));

        let mut without_tags = head_page(BOS);
        without_tags.extend(page(1, 0, 0, &[&junk(436)]));
        without_tags.extend(page(2, u64::from(SAMPLES), 0, &[&junk(FIRST_AUDIO_PACKET)]));

        for (audio, page) in [(without_head, 0), (without_tags, 1)] {
            let block = header_of(&audio);
            let header = HeaderView::parse(&block).unwrap();

            assert_eq!(
                validate(&header, &audio).refused,
                Some((0, ValidateError::MissingOpusHeader { page })),
                "page {page}"
            );
        }
    }

    #[test]
    fn reports_pages_of_another_stream() {
        let audio = audio(1);
        // The audio region of one file and the header block of another.
        let block = encode_header(
            &sha1_of(&audio),
            u32::try_from(audio.len()).unwrap(),
            AudioId::new(AUDIO_ID.get() + 1),
            &[0],
        )
        .unwrap();
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((0, ValidateError::SerialMismatch))
        );
    }

    #[test]
    fn reports_a_sequence_number_that_does_not_follow_the_page_before_it() {
        let mut audio = first_block();
        audio.extend(page(4, 2 * u64::from(SAMPLES), 0, &[&junk(PAGE_PACKET)]));

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((1, ValidateError::SequenceGap { expected: 3 }))
        );
    }

    #[test]
    fn reports_a_granule_position_that_goes_backwards() {
        let mut audio = first_block();
        // The first audio page states 2880 samples, and this one states fewer than that.
        audio.extend(page(3, 0, 0, &[&junk(PAGE_PACKET)]));

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((1, ValidateError::GranuleRegression { page: 3 }))
        );
    }

    #[test]
    fn reports_a_block_that_does_not_end_where_a_page_does() {
        // A first block whose first page fills the whole of it: a TAF's first block holds three
        // pages, and no page but the last of a block ends where the block does.
        let one_page = page(0, 0, BOS, &[&opus_head(OPUS_PRE_SKIP), &junk(4033)]);

        // A first block whose three pages leave a byte of it over.
        let mut short_first = head_page(BOS);
        short_first.extend(tags_page());
        short_first.extend(page(2, u64::from(SAMPLES), 0, &[&junk(3542)]));
        short_first.push(0);

        // A first block of exactly the right size whose two Opus header pages span 647 bytes
        // rather than 512: a box seeks the first chapter to 4096 + 0x200 and would land 135 bytes
        // inside this file's second page, so the chapter it seeks to is not there. Everything else
        // about the block adds up — three pages, the last of them closing the block.
        let head = head_page(BOS);
        let wide_tags = page(1, 0, 0, &[&opus_tags("taffle", &[]).unwrap(), &junk(134)]);

        assert_eq!((head.len(), wide_tags.len()), (47, 600));

        let mut wide_header_pages = head;
        wide_header_pages.extend(wide_tags);
        wide_header_pages.extend(page(2, u64::from(SAMPLES), 0, &[&junk(3408)]));

        // And a block behind it that holds two pages rather than one.
        let mut two_pages = first_block();
        two_pages.extend(page(3, 2 * u64::from(SAMPLES), 0, &[&junk(2013)]));
        two_pages.extend(page(4, 3 * u64::from(SAMPLES), 0, &[&junk(2013)]));

        let cases = [
            (one_page, 0, ValidateError::Misaligned { page: 0 }),
            (wide_header_pages, 0, ValidateError::Misaligned { page: 1 }),
            (short_first, 0, ValidateError::Misaligned { page: 2 }),
            (two_pages, 1, ValidateError::Misaligned { page: 3 }),
        ];

        for (audio, at, expected) in cases {
            assert_eq!(audio.len() % BLOCK_LEN, 0, "{expected:?}");

            let block = header_of(&audio);
            let header = HeaderView::parse(&block).unwrap();

            assert_eq!(
                validate(&header, &audio).refused,
                Some((at, expected)),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn reports_a_page_that_reaches_past_the_block_it_lies_in() {
        // A block-aligned page stating one lacing value more than it has: the segments it
        // describes then reach past the end of the block, which is what a page crossing a block
        // boundary comes to — and a block is all a reader hands the page reader.
        let mut audio = first_block();
        let mut spanning = audio_block(1, 0);
        spanning[SEGMENTS_AT] += 1;
        reseal(&mut spanning);

        assert_eq!(spanning.len(), BLOCK_LEN);
        audio.extend(spanning);

        let block = header_of(&audio);
        let header = HeaderView::parse(&block).unwrap();

        assert_eq!(
            validate(&header, &audio).refused,
            Some((1, ValidateError::Page(PageError::TruncatedBody)))
        );
    }

    #[test]
    fn hands_over_every_chapter_as_the_block_it_starts_goes_by() {
        let audio = audio(2);
        let block = header_block(&audio, &[0, 1, 2]);
        let header = HeaderView::parse(&block).unwrap();
        let run = validate(&header, &audio);

        assert_eq!(run.refused, None);
        assert_eq!(run.summary.map(|summary| summary.chapters_seen), Ok(3));
        // The audio before a chapter is what the pages before its block carried.
        assert_eq!(
            run.chapters,
            [
                ChapterInfo {
                    block: BlockIndex::new(0),
                    granule: 0
                },
                ChapterInfo {
                    block: BlockIndex::new(1),
                    granule: u64::from(SAMPLES)
                },
                ChapterInfo {
                    block: BlockIndex::new(2),
                    granule: 2 * u64::from(SAMPLES)
                },
            ]
        );
    }

    #[test]
    fn reports_a_chapter_the_file_does_not_hold() {
        let audio = audio(2);

        // A chapter past the end of the file, and one the walk has already gone past — chapters
        // are matched against the blocks in the order the header lists them.
        for (chapters, missing) in [(&[0, 3][..], 3), (&[0, 2, 1][..], 1)] {
            let block = header_block(&audio, chapters);
            let header = HeaderView::parse(&block).unwrap();
            let run = validate(&header, &audio);

            assert_eq!(run.refused, None, "chapters {chapters:?}");
            assert_eq!(
                run.summary,
                Err(ValidateError::ChapterPageMissing(missing)),
                "chapters {chapters:?}"
            );
        }
    }

    #[test]
    fn reports_a_stream_that_never_begins() {
        // A header stating an audio region of no bytes at all: there is nothing to be short of,
        // and no page that opens the stream either.
        let block = encode_header(&sha1_of(&[]), 0, AUDIO_ID, &[]).unwrap();
        let header = HeaderView::parse(&block).unwrap();
        let run = validate(&header, &[]);

        assert_eq!(run.refused, None);
        assert_eq!(run.summary, Err(ValidateError::MissingBos));
    }

    #[test]
    fn refuses_a_file_whose_audio_region_is_not_whole_blocks() {
        // A file with no audio at all: the two pages the Opus stream opens with come to 512 bytes,
        // which is an eighth of a block, and that is the whole audio region. teddycloud's own
        // writer leaves the same file behind.
        let pages = Pages::default();
        let writer = TafWriter::new(digest(), AUDIO_ID, tags(), |page: &[u8]| {
            pages.borrow_mut().push(page.to_vec());
        })
        .unwrap();
        let block = writer.finalize().unwrap();
        let header = HeaderView::parse(&block).unwrap();
        let audio = pages.borrow().concat();

        assert_eq!(audio.len(), 512);
        assert_eq!(header.data_length(), 512);

        let run = validate(&header, &audio);

        assert_eq!(
            run.refused,
            Some((0, ValidateError::WrongBlockLen { len: 512 }))
        );
        assert_eq!(
            run.summary,
            Err(ValidateError::LengthMismatch {
                header: 512,
                actual: 0
            })
        );
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        let rendered = [
            ValidateError::Page(PageError::BadCrc),
            ValidateError::WrongBlockLen { len: 512 },
            ValidateError::Misaligned { page: 7 },
            ValidateError::SerialMismatch,
            ValidateError::SequenceGap { expected: 3 },
            ValidateError::GranuleRegression { page: 7 },
            ValidateError::MissingBos,
            ValidateError::UnexpectedBos { page: 7 },
            ValidateError::UnexpectedEos { page: 7 },
            ValidateError::ContinuedPacket { page: 7 },
            ValidateError::MissingOpusHeader { page: 1 },
            ValidateError::LengthMismatch {
                header: 110_592,
                actual: 106_496,
            },
            ValidateError::Sha1Mismatch,
            ValidateError::ChapterPageMissing(27),
        ]
        .map(|error| alloc::format!("{error}"));

        assert_eq!(
            rendered,
            [
                "an Ogg page's checksum does not match its bytes",
                "a TAF's audio region is read one 4096-byte block at a time, and 512 bytes are not one",
                "Ogg page 7 does not end where the TAF block it lies in does",
                "an Ogg page states a serial number other than the file's audio id",
                "an Ogg page states a sequence number other than 3",
                "Ogg page 7 states a granule position behind the page before it",
                "a TAF's Opus stream never begins",
                "Ogg page 7 begins a stream that page 0 already began",
                "Ogg page 7 ends the stream, which no page of a TAF does",
                "Ogg page 7 carries on a packet from the page before it, which no page of a TAF does",
                "Ogg page 1 does not carry the Opus header packet it opens the stream with",
                "a TAF's audio region is 106496 bytes, and its header states 110592",
                "a TAF's audio region does not hash to what its header states",
                "a TAF's header starts a chapter at block 27, which the file does not hold",
            ]
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn validate_error_is_a_standard_error_that_names_what_it_wraps() {
        use std::error::Error;

        let wrapping = ValidateError::Page(PageError::BadCrc);
        let plain = ValidateError::Sha1Mismatch;

        assert_eq!(
            std::string::ToString::to_string(&plain),
            "a TAF's audio region does not hash to what its header states"
        );
        assert!(plain.source().is_none());
        assert_eq!(
            std::string::ToString::to_string(&wrapping.source().unwrap()),
            "an Ogg page's checksum does not match its bytes"
        );
    }

    #[test]
    fn the_constants_are_the_ones_the_format_states() {
        assert_eq!(usize::try_from(BLOCK_BYTES), Ok(BLOCK_LEN));
        assert_eq!(BLOCK_LEN, 4096);
        assert_eq!(FIRST_BLOCK_PAGES, 3);
        assert_eq!(BLOCK_PAGES, 1);
        assert_eq!(HEADER_PAGES, 2);
        // The 0x200 a box adds to the offset it seeks the first chapter to, which is what the two
        // pages the golden file opens with span: 47 + 465.
        assert_eq!(HEADER_PAGES_LEN, 512);
        assert_eq!(
            PageView::parse(&GOLDEN[AUDIO_AT..]).unwrap().total_len()
                + PageView::parse(&GOLDEN[AUDIO_AT + 47..])
                    .unwrap()
                    .total_len(),
            HEADER_PAGES_LEN
        );
    }
}
