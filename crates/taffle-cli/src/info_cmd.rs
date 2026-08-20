//! Reading TAF files back: what a file says about itself, held against what it holds.
//!
//! The reading is `taffle`'s — the header block, then the audio region one 4096-byte block at a
//! time, hashed on the way — so what is printed here has been checked rather than believed. What
//! is here is what a command line does with it: which files are read, what is said about each of
//! them, and what the run as a whole comes to.
//!
//! # What is said where
//!
//! A file that is the one its header describes is reported on stdout, all of it. A file that is
//! not gets no report at all — everything there would be to say about it comes out of a header the
//! file has just been shown to be lying in — and is said on stderr instead, one line naming it and
//! what went wrong. The files behind it are read all the same, and the run's code is 1 if any of
//! them was refused.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use taffle::{inspect, InspectError, Inspection};

use crate::duration::clock;

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
                eprintln!("{}: {:#}", path.display(), refusal(why));
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

/// Says what `inspection` found in the file at `path`.
///
/// The chapter table names the *block* every chapter starts at, which is what a box seeks on and
/// what the header holds — block `n` begins at file offset `4096 * (n + 1)`. The header's own field
/// for it is called `chapterPages`, and this crate's word throughout is the one `FORMAT.md` uses.
fn report(path: &Path, inspection: &Inspection) {
    let &Inspection {
        audio_id,
        duration,
        audio_bytes,
        ref chapters,
    } = inspection;

    println!("{}", path.display());
    println!("  audio id: {}", audio_id.get());
    println!("  duration: {}", clock(duration));
    println!("  audio: {audio_bytes} bytes");
    println!("  chapters: {}", chapters.len());
    // A number wider than its column takes the room it needs: the columns line a table up, they do
    // not cut anything short.
    println!("    {:>3}  {:>5}  {:>5}", "#", "block", "start");

    for (at, chapter) in chapters.iter().enumerate() {
        println!(
            "    {:>3}  {:>5}  {:>5}",
            at + 1,
            chapter.block.get(),
            clock(chapter.start)
        );
    }

    println!("  valid");
}

/// What to say about a file that is not the TAF its header describes.
///
/// Everything but the hash renders as it stands, every layer of it. The hash is this frontend's to
/// put into words: the reading states the hash the header asks for and the one the audio came to,
/// and saying both is the difference between "somewhere in these megabytes" and two hashes to
/// compare.
fn refusal(error: InspectError) -> anyhow::Error {
    match error {
        InspectError::Sha1Mismatch { stated, hashed } => anyhow!(
            "the audio does not hash to the sha1 its header states: header {}, audio {}",
            hex(&stated),
            hex(&hashed)
        ),
        other => anyhow::Error::new(other),
    }
}

/// `bytes` in the hexadecimal a hash is read and quoted in.
fn hex(bytes: &[u8; SHA1_LEN]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        // Writing into a string cannot fail, and a hash half-written is still what is shown.
        let _ = write!(text, "{byte:02x}");

        text
    })
}
