//! Reading a TAF back: what a file says about itself, held against what it holds.
//!
//! A file is read through the way a box reads one — the header block, then the audio region one
//! 4096-byte block at a time, hashed on the way — so what comes back out of [`inspect`] has been
//! checked rather than believed. Nothing is held but the block in hand and the chapter table being
//! built, so a file of any length is read in the same handful of kilobytes.
//!
//! # Nothing is seeked, and what that is for
//!
//! [`read_through`] takes anything bytes come out of in order: a file, a pipe, a download in
//! progress. That is what `taf`'s own reader is shaped for — a box reads a TAF forwards, and the
//! digest it hashes with is fed the same way — and it is why a frontend that has no file yet can
//! still say whether what it is being handed is one.
//!
//! # Where the hash comes from
//!
//! `taf` states what it needs from a SHA-1 and takes the implementation from whoever uses it: a
//! software crate on a host, a hardware peripheral on a microcontroller. This is that
//! implementation for a host, and it is the reason a caller of this crate hashes nothing itself.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

// The digest is fed and finalized through `taf`'s own interface for one; the name stays out of
// scope, since `sha1` has one of its own here.
use taf::digest::Sha1 as _;
use taf::header::{HeaderError, HeaderView, BLOCK_LEN};
use taf::id::{AudioId, BlockIndex};
use taf::ogg::OPUS_PRE_SKIP;
use taf::reader::{ValidateError, Validator};

/// The bytes one block occupies, in the width a file is read in.
const BLOCK_BYTES: u64 = BLOCK_LEN as u64;

/// The bytes a SHA-1 comes to, which is what a TAF header states its audio hashes to.
const SHA1_LEN: usize = 20;

/// The frames of one channel a second of a TAF's audio comes to.
const RATE: u64 = 48_000;

/// What a file holds, once it has been read through and found to be what its header describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// The file's audio id, which is also the serial number of its every Ogg page.
    pub audio_id: AudioId,
    /// How long the file plays.
    pub duration: Duration,
    /// The bytes its audio region occupies, which is everything behind the header block.
    pub audio_bytes: u32,
    /// Where every chapter of it starts, in the order the file holds them.
    pub chapters: Vec<ChapterRead>,
}

/// One chapter of a file that was read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterRead {
    /// The block of the audio region the chapter starts at, which is what a box seeks on and what
    /// the header holds — block `n` begins at file offset `4096 * (n + 1)`.
    pub block: BlockIndex,
    /// How far into the audio it starts.
    pub start: Duration,
}

/// Reads the TAF at `path`.
///
/// # Errors
///
/// [`InspectError::Open`] if the file cannot be opened, and whatever [`read_through`] makes of
/// what is in it.
pub fn inspect(path: &Path) -> Result<Inspection, InspectError> {
    let mut file = File::open(path).map_err(InspectError::Open)?;

    read_through(&mut file)
}

/// Reads a TAF out of `source` block by block, hashing it as it goes.
///
/// What is read is what a box reads: the header block, and behind it the audio region in blocks of
/// its own. Nothing is seeked and nothing but the block in hand is held, so this works on a file, a
/// pipe or anything else bytes come out of in order — which is what it takes them as, the way `taf`
/// takes the digest it hashes with.
///
/// # Errors
///
/// If `source` cannot be read; if its first block is no TAF header; or if what lies behind that
/// block is not the audio region the header describes — its framing, its length, its chapters or
/// its hash.
pub fn read_through(source: &mut dyn Read) -> Result<Inspection, InspectError> {
    let mut first = Vec::with_capacity(BLOCK_LEN);
    read_block(source, &mut first).map_err(InspectError::Header)?;

    let header = HeaderView::parse(&first)?;
    let mut validator = Validator::new(&header);
    let mut digest = Digest::new();
    let mut chapters = Vec::new();
    let mut block = Vec::with_capacity(BLOCK_LEN);
    let mut at = 0usize;

    while read_block(source, &mut block).map_err(InspectError::Read)? > 0 {
        let started = validator
            .push_block(&block, Some(&mut digest))
            .map_err(|source| InspectError::Block { at, source })?;
        chapters.extend(started.map(|chapter| ChapterRead {
            block: chapter.block,
            start: playtime(chapter.granule),
        }));
        at += 1;
    }

    let hashed = digest.finalize();
    let summary = validator
        .finish(Some(hashed))
        .map_err(|error| refused(error, header.sha1(), hashed))?;

    Ok(Inspection {
        audio_id: header.audio_id(),
        duration: playtime(summary.total_samples),
        audio_bytes: summary.audio_bytes,
        chapters,
    })
}

/// Why a file is not the TAF its header describes, or not one at all.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InspectError {
    /// The file could not be opened.
    #[error(transparent)]
    Open(io::Error),
    /// The block the header lies in could not be read.
    #[error("cannot read the header block")]
    Header(#[source] io::Error),
    /// That block is no TAF header.
    #[error(transparent)]
    Parse(#[from] HeaderError),
    /// The audio region could not be read to its end: a source that stopped being readable is not
    /// a file that ended, and half a book taken for a whole one is the one answer never to give.
    #[error(transparent)]
    Read(io::Error),
    /// A block of the audio region is not the one the header describes — its framing, its serial
    /// number, its checksum or the chapter it starts.
    #[error("block {at} of the audio region")]
    Block {
        /// Which block of the audio region it was, counted from the one behind the header.
        at: usize,
        /// What was wrong with it.
        #[source]
        source: ValidateError,
    },
    /// The audio region as a whole is not the one the header describes: its length, or how many
    /// chapters it turned out to hold.
    #[error(transparent)]
    Audio(ValidateError),
    /// The audio does not hash to the SHA-1 its header states, which is what audio somebody
    /// changed and re-summed the pages of comes to.
    ///
    /// The two hashes travel with it because this is what hashed the file: whoever renders this can
    /// state what the audio came to beside what the header asks for, which is the difference
    /// between "somewhere in these megabytes" and two hashes to compare.
    #[error("the audio does not hash to the sha1 its header states")]
    Sha1Mismatch {
        /// The hash the header states.
        stated: [u8; SHA1_LEN],
        /// The hash the audio came to.
        hashed: [u8; SHA1_LEN],
    },
}

/// What an audio region that is not the one its header describes comes back as.
///
/// Everything but the hash is the reader's own word, carried as it stands. The hash is this
/// crate's to state, since this is what hashed the file.
fn refused(error: ValidateError, stated: &[u8; SHA1_LEN], hashed: [u8; SHA1_LEN]) -> InspectError {
    match error {
        ValidateError::Sha1Mismatch => InspectError::Sha1Mismatch {
            stated: *stated,
            hashed,
        },
        other => InspectError::Audio(other),
    }
}

/// How far into a file's audio a granule position is, at the 48 kHz a TAF carries.
///
/// A granule position counts the Opus pre-skip in with the audio, and the pre-skip is not audio, so
/// it comes off first. It is 312 samples — six and a half milliseconds, which never shows at the
/// seconds a clock is read in, but it is what the format says a granule position means.
///
/// Whole seconds and the nanoseconds left over, so that nothing is lost to a float on the way: one
/// sample is 20 833 and a third nanoseconds, and what the division leaves of that is under a
/// nanosecond of the answer.
fn playtime(granule: u64) -> Duration {
    let samples = granule.saturating_sub(u64::from(OPUS_PRE_SKIP));
    let rest = (samples % RATE) * 1_000_000_000 / RATE;

    Duration::new(samples / RATE, u32::try_from(rest).unwrap_or(0))
}

/// Reads the next block out of `source` into `block`, and says how many bytes there were: a whole
/// block, what was left of the file at its end, or nothing at all where it ended on a block
/// boundary.
///
/// # Errors
///
/// If `source` could not be read.
fn read_block(source: &mut dyn Read, block: &mut Vec<u8>) -> io::Result<usize> {
    block.clear();

    source.take(BLOCK_BYTES).read_to_end(block)
}

/// The digest a TAF's audio region is hashed with as it goes past.
struct Digest(sha1::Sha1);

impl Digest {
    /// A digest with nothing in it yet.
    fn new() -> Self {
        Self(<sha1::Sha1 as sha1::Digest>::new())
    }
}

impl taf::digest::Sha1 for Digest {
    fn update(&mut self, data: &[u8]) {
        sha1::Digest::update(&mut self.0, data);
    }

    fn finalize(self) -> [u8; SHA1_LEN] {
        sha1::Digest::finalize(self.0).into()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::io::Cursor;
    use std::time::Duration;

    use taf::header::encode_header;
    use taf::id::AudioId;

    use super::{playtime, read_through, InspectError, Read, BLOCK_LEN, SHA1_LEN};

    /// What a file that stopped being readable says when it is read from.
    const GONE: &str = "the disk went away";

    /// A file that reads as far as what it was given goes, and fails from there on.
    ///
    /// Which is the difference a reader has to keep: a read that fails is not a file that ended,
    /// and half a book taken for a whole one is the one answer an inspection must never give.
    struct Failing(Cursor<Vec<u8>>);

    impl Read for Failing {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            // Reading bytes already in hand cannot fail, so what comes back is what was left.
            match self.0.read(buffer).unwrap_or_default() {
                0 => Err(std::io::Error::other(GONE)),
                read => Ok(read),
            }
        }
    }

    /// A header block stating an audio region of one block, which is what a file that reads past
    /// its header has to have behind it.
    fn header_block() -> Vec<u8> {
        encode_header(&[0; SHA1_LEN], 4096, AudioId::new(1), &[0])
            .expect("a header block")
            .to_vec()
    }

    #[test]
    fn a_file_that_cannot_be_read_at_all_says_so_rather_than_that_it_is_no_taf() {
        let why = read_through(&mut Failing(Cursor::new(Vec::new())))
            .expect_err("a file that cannot be read is no file to read");

        // The read that failed, carried as it stands: a file nothing could be read out of is not a
        // file that was read and found to be no TAF.
        assert!(
            matches!(&why, InspectError::Header(read) if read.to_string().contains(GONE)),
            "{why:?}"
        );
    }

    #[test]
    fn a_file_that_stops_being_readable_is_said_so_rather_than_taken_for_its_end() {
        // The header block reads, and the audio region behind it does not. A file that ended there
        // would be short of what its header states; this one is a file nobody knows the end of, and
        // saying which is the whole point of telling the two apart.
        let why = read_through(&mut Failing(Cursor::new(header_block())))
            .expect_err("a file that cannot be read is no file to read");

        assert!(
            matches!(&why, InspectError::Read(read) if read.to_string().contains(GONE)),
            "{why:?}"
        );
        assert_eq!(header_block().len(), BLOCK_LEN);
    }

    #[test]
    fn a_granule_position_is_the_audio_in_front_of_it_and_not_the_pre_skip() {
        // The first sample of the audio is granule 312, which is where the file begins playing.
        assert_eq!(playtime(0), Duration::ZERO);
        assert_eq!(playtime(312), Duration::ZERO);
        assert_eq!(playtime(48_312), Duration::from_secs(1));
        // And what is not a whole second is the nanoseconds of one, kept rather than dropped: 2 880
        // samples is one Opus frame of a TAF, which is 60 ms.
        assert_eq!(playtime(2_880 + 312), Duration::from_millis(60));
        assert_eq!(playtime(48_312 + 1), Duration::new(1, 20_833));
    }
}
