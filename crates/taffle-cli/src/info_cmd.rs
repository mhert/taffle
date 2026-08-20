//! Reading TAF files back: what a file says about itself, held against what it holds.
//!
//! Every file is read through the way a box reads one — the header block, then the audio region
//! one 4096-byte block at a time, hashed on the way — so what is printed about it has been
//! checked rather than believed. Nothing is held but the block being read and the chapter table
//! being built, so a file of any length is inspected in the same handful of kilobytes.
//!
//! # What is said where
//!
//! A file that is the one its header describes is reported on stdout, all of it. A file that is
//! not gets no report at all — everything there would be to say about it comes out of a header the
//! file has just been shown to be lying in — and is said on stderr instead, one line naming it and
//! what went wrong. The files behind it are read all the same, and the run's code is 1 if any of
//! them was refused.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
// The digest is fed and finalized through `taf`'s own interface for one; the name stays out of
// scope, since `sha1` has one of its own here.
use taf::digest::Sha1 as _;
use taf::header::{HeaderView, BLOCK_LEN};
use taf::id::AudioId;
use taf::ogg::OPUS_PRE_SKIP;
use taf::reader::{ChapterInfo, Summary, ValidateError, Validator};

use crate::duration::{clock, RATE};

/// The bytes one block occupies, in the width a file is read in.
const BLOCK_BYTES: u64 = BLOCK_LEN as u64;

/// The bytes a SHA-1 comes to, which is what a TAF header states its audio hashes to.
const SHA1_LEN: usize = 20;

/// Reads every file `files` names, and says what each of them holds.
///
/// The files are read in the order they were named, and one that is refused does not stop the ones
/// behind it: a run over a directory of books says what is wrong with each of them in one go.
///
/// # Errors
///
/// If any file is not the TAF its header describes. What was wrong with each of them has been said
/// on stderr by then; this is what the run as a whole came to.
pub fn run(files: &[PathBuf]) -> Result<()> {
    let mut refused = 0usize;

    for path in files {
        match inspect(path) {
            Ok(inspection) => report(path, &inspection),
            Err(why) => {
                eprintln!("{}: {why:#}", path.display());
                refused += 1;
            }
        }
    }

    match refused {
        0 => Ok(()),
        1 => Err(anyhow!("1 file is not the TAF its header describes")),
        many => Err(anyhow!(
            "{many} files are not the TAFs their headers describe"
        )),
    }
}

/// What a file holds, once it has been read through and found to be what its header describes.
#[derive(Debug)]
struct Inspection {
    /// The file's audio id, which is also the serial number of its every Ogg page.
    audio_id: AudioId,
    /// What its audio region came to.
    summary: Summary,
    /// Where every chapter of it starts, in the order the file holds them.
    chapters: Vec<ChapterInfo>,
}

/// Reads the TAF at `path`.
///
/// # Errors
///
/// If the file cannot be opened or read; if its first block is no TAF header; or if what lies
/// behind that block is not the audio region the header describes — its framing, its length, its
/// chapters or its hash.
fn inspect(path: &Path) -> Result<Inspection> {
    let mut file = File::open(path)?;

    read_through(&mut file)
}

/// Reads a TAF out of `source` block by block, hashing it as it goes.
///
/// What is read is what a box reads: the header block, and behind it the audio region in blocks of
/// its own. Nothing is seeked and nothing but the block in hand is held, so this works on a file, a
/// pipe or anything else bytes come out of in order — which is what it takes them as, the way
/// `taf` takes the digest it hashes with.
///
/// # Errors
///
/// If `source` cannot be read; if its first block is no TAF header; or if what lies behind that
/// block is not the audio region the header describes.
fn read_through(source: &mut dyn Read) -> Result<Inspection> {
    let mut first = Vec::with_capacity(BLOCK_LEN);
    read_block(source, &mut first).context("cannot read the header block")?;

    let header = HeaderView::parse(&first)?;
    let mut validator = Validator::new(&header);
    let mut digest = Digest::new();
    let mut chapters = Vec::new();
    let mut block = Vec::with_capacity(BLOCK_LEN);
    let mut at = 0usize;

    while read_block(source, &mut block)? > 0 {
        let chapter = validator
            .push_block(&block, Some(&mut digest))
            .with_context(|| format!("block {at} of the audio region"))?;
        chapters.extend(chapter);
        at += 1;
    }

    let hashed = digest.finalize();
    let summary = validator
        .finish(Some(hashed))
        .map_err(|error| refusal(error, header.sha1(), &hashed))?;

    Ok(Inspection {
        audio_id: header.audio_id(),
        summary,
        chapters,
    })
}

/// Says what `inspection` found in the file at `path`.
///
/// The chapter table names the *block* every chapter starts at, which is what a box seeks on and
/// what the header holds — block `n` begins at file offset `4096 * (n + 1)`. The header's own field
/// for it is called `chapterPages`, and this crate's word throughout is the one `FORMAT.md` uses.
fn report(path: &Path, inspection: &Inspection) {
    let &Inspection {
        audio_id,
        summary,
        ref chapters,
    } = inspection;

    println!("{}", path.display());
    println!("  audio id: {}", audio_id.get());
    println!("  duration: {}", clock(playtime(summary.total_samples)));
    println!("  audio: {} bytes", summary.audio_bytes);
    println!("  chapters: {}", chapters.len());
    // A number wider than its column takes the room it needs: the columns line a table up, they do
    // not cut anything short.
    println!("    {:>3}  {:>5}  {:>5}", "#", "block", "start");

    for (at, chapter) in chapters.iter().enumerate() {
        println!(
            "    {:>3}  {:>5}  {:>5}",
            at + 1,
            chapter.block.get(),
            clock(playtime(chapter.granule))
        );
    }

    println!("  valid");
}

/// What to say about an audio region that is not the one its header describes.
///
/// Everything but the hash is the reader's own word, said as it stands. The hash is this frontend's
/// to say: it is the one that hashed the file, so it is the one that can state what the audio came
/// to beside what the header asks for — which is the difference between "somewhere in these
/// megabytes" and two hashes to compare.
fn refusal(
    error: ValidateError,
    stated: &[u8; SHA1_LEN],
    hashed: &[u8; SHA1_LEN],
) -> anyhow::Error {
    match error {
        ValidateError::Sha1Mismatch => anyhow!(
            "the audio does not hash to the sha1 its header states: header {}, audio {}",
            hex(stated),
            hex(hashed)
        ),
        other => anyhow::Error::new(other),
    }
}

/// How far into a file's audio a granule position is, at the 48 kHz a TAF carries.
///
/// A granule position counts the Opus pre-skip in with the audio, and the pre-skip is not audio, so
/// it comes off first. It is 312 samples — six and a half milliseconds, which never shows at the
/// seconds a clock is read in, but it is what the format says a granule position means.
fn playtime(granule: u64) -> Duration {
    let samples = granule.saturating_sub(u64::from(OPUS_PRE_SKIP));

    Duration::from_secs(samples / u64::from(RATE))
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

/// `bytes` in the hexadecimal a hash is read and quoted in.
fn hex(bytes: &[u8; SHA1_LEN]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing into a string cannot fail, and a hash half-written is still what is shown.
        let _ = write!(text, "{byte:02x}");

        text
    })
}

/// The digest a TAF's audio region is hashed with as it goes past.
///
/// `taf` states what it needs from a SHA-1 and takes the implementation from whoever uses it; this
/// is that implementation for a host, which is the `sha1` crate.
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

    use taf::header::encode_header;
    use taf::id::AudioId;

    use super::{read_through, Read, BLOCK_LEN, SHA1_LEN};

    /// What a file that stopped being readable says when it is read from.
    const GONE: &str = "the disk went away";

    /// A file that reads as far as what it was given goes, and fails from there on.
    ///
    /// Which is the difference a reader has to keep: a read that fails is not a file that ended,
    /// and half a book taken for a whole one is the one answer `info` must never give.
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

        assert!(format!("{why:#}").contains(GONE), "{why:#}");
    }

    #[test]
    fn a_file_that_stops_being_readable_is_said_so_rather_than_taken_for_its_end() {
        // The header block reads, and the audio region behind it does not. A file that ended there
        // would be short of what its header states; this one is a file nobody knows the end of, and
        // saying which is the whole point of telling the two apart.
        let why = read_through(&mut Failing(Cursor::new(header_block())))
            .expect_err("a file that cannot be read is no file to read");

        assert!(format!("{why:#}").contains(GONE), "{why:#}");
        assert_eq!(header_block().len(), BLOCK_LEN);
    }
}
