//! File-oriented workflows shared by any frontend (CLI today, GUI anticipated): converting audio
//! into a TAF, and reading one back.
//!
//! The engine below this takes readers and hands back a report; a person converting an audiobook
//! has files, and wants the one that comes out named after the one that went in with its cover
//! beside it. That gap is the whole of this crate: paths in, [`taf_encode::convert()`] in the
//! middle, a `.taf` and a picture out. Nothing here decodes, encodes or decides anything about the
//! audio.
//!
//! [`inspect`] is the same gap on the way back: a path in, and what the file holds out — read
//! through block by block and hashed on the way, so that what a frontend shows has been checked
//! rather than believed.
//!
//! [`duration`] touches no file: the grammar a length of audio is typed in and the clock it is
//! shown back on. It sits below the frontends rather than in one of them, so that all of them read
//! and write a duration the same way.
//!
//! [`refuse_collisions`], [`planned_chapters`] and [`MAX_CHAPTERS`] are here for that same reason:
//! what a frontend settles before it converts anything — jobs that would write over what they read
//! or over each other, a chapter list longer than a box plays — is settled once here, so that every
//! frontend refuses and warns about the same things.
//!
//! [`probe_duration`] barely touches a file: how long it states it plays, read off its headers and
//! without decoding a packet of it, so that a frontend can show the length of what it is about to
//! convert before it converts any of it.
//!
//! # A frontend depends on this crate and no other
//!
//! What these workflows hand back is made of `taf-encode`'s types and `taf`'s, so those are
//! re-exported here in full: a frontend states its conversion in [`Conversion`], renders
//! [`Progress`] as it runs and reads a [`ConversionReport`] at the end without naming either crate
//! below this one. Which is what keeps a frontend from depending on a version of them that is not
//! the version these workflows are built on.
//!
//! # The one clock read
//!
//! A TAF carries an audio id, which teddycloud writes a timestamp into and which every Ogg page of
//! the file states as its serial number. An engine that read a clock could not be held to a fixed
//! file, so the engine takes that id as a parameter — and this is where it comes from: the current
//! Unix time, in seconds, read in [`run_convert`] and handed over. It is the only clock read in
//! taffle, and deliberately the only one: everything below it is a function of its inputs.
//!
//! # A cover never fails a conversion
//!
//! The cover is a file *beside* the file, and a book that converted is converted whether or not the
//! picture next to it could be written. So every way the cover can come to nothing — a type nothing
//! here writes, a directory that refuses the file — comes back in [`JobOutcome::cover_error`] with
//! the conversion's own report beside it, and never as an [`Err`].

mod collision;
mod cover;
mod inspect;
mod output;

pub mod duration;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use taf_encode::{convert, Input};

pub use collision::{refuse_collisions, CollisionError};
pub use inspect::{inspect, read_through, ChapterRead, InspectError, Inspection};
pub use output::default_output_path;

// What the workflows above hand back and take in, from the crates the types are defined in: a
// frontend names them through here rather than depending on `taf` and `taf-encode` itself.
pub use taf::header::HeaderError;
pub use taf::id::{AudioId, BlockIndex};
pub use taf::reader::ValidateError;
pub use taf_encode::{
    ChapterError, ChapterMode, ChapterOut, Conversion, ConversionReport, ConvertError, Cover,
    ProbeError, Progress, SilenceOpts,
};

/// A conversion of files on disk: what goes in, where it comes out, and what is done to the audio
/// on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertJob {
    /// The audio files to convert, in the order they play.
    pub inputs: Vec<PathBuf>,
    /// Where the TAF goes, or [`None`] for [`default_output_path`] of the first input.
    pub output: Option<PathBuf>,
    /// What the engine is asked to do with the audio.
    pub options: Conversion,
    /// Whether the cover art an input carried is written beside the TAF.
    pub write_cover: bool,
}

/// What a job came to: the files it left on the disk, and what the conversion itself reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    /// The TAF that was written.
    pub taf_path: PathBuf,
    /// The cover beside it, where one was written. Never [`taf_path`](Self::taf_path): the picture
    /// is written beside the book and never over it.
    pub cover_path: Option<PathBuf>,
    /// Why no cover was written, where one was carried and asked for and did not make it.
    ///
    /// This and [`cover_path`](Self::cover_path) are never both set: a cover was written, or it
    /// says why it was not, or neither — which is a job that asked for no cover, or inputs that
    /// carried none.
    ///
    /// An output the caller named `Book.png`, converted from a book carrying a PNG, is one of the
    /// ways a cover comes to nothing: the picture's place is the converted file itself, so it is
    /// left out and says so, and the book that was just written stands.
    pub cover_error: Option<String>,
    /// What the conversion itself came to.
    pub report: ConversionReport,
}

/// Converts the files `job` names into a TAF, and writes the cover art beside it.
///
/// The inputs are opened in the order they are stated and in front of the output, so a job naming a
/// file that is not there leaves nothing behind at all. `progress` is the engine's own, handed
/// through as it comes — and so is the answer it gives back, which is what stops a conversion that
/// is running.
///
/// # Errors
///
/// - [`JobError::OpenInput`] if an input could not be opened.
/// - [`JobError::CreateOutput`] if the TAF could not be created.
/// - [`JobError::Convert`] if the conversion itself failed. A job of no inputs is one of those,
///   refused as [`ChapterError::Empty`] before a file is made for it: what the engine calls having
///   nothing to convert is what a caller is handed here, rather than a second name for it.
/// - [`JobError::Convert`] carrying [`ConvertError::Cancelled`] if `progress` asked the conversion
///   to stop.
///
/// A conversion that failed part-way — a cancelled one among them — leaves the output file behind
/// holding what was written before it failed, which is [`taf_encode::convert()`]'s business and
/// stated there.
///
/// Failing to write the cover is *not* one of these; it comes back in
/// [`JobOutcome::cover_error`].
pub fn run_convert(
    job: ConvertJob,
    progress: &mut dyn FnMut(Progress) -> std::ops::ControlFlow<()>,
) -> Result<JobOutcome, JobError> {
    let ConvertJob {
        inputs,
        output,
        options,
        write_cover,
    } = job;

    // Nothing to convert is nothing to name the output after either, and the engine has the word
    // for it.
    let Some(first) = inputs.first() else {
        return Err(JobError::Convert(ChapterError::Empty.into()));
    };
    let taf_path = output.unwrap_or_else(|| default_output_path(first));

    let sources = opened(&inputs)?;
    let out = File::create(&taf_path).map_err(|source| JobError::CreateOutput {
        path: taf_path.clone(),
        source,
    })?;
    // The engine writes the file in many small pieces, and a buffer in front of it turns those
    // into few whole writes rather than a syscall apiece. Finishing the file seeks back to fill in
    // the header block and flushes, so a buffered write that failed is reported by the conversion
    // and not left for the drop to swallow.
    let mut out = std::io::BufWriter::new(out);
    let report = convert(sources, &options, clock_audio_id(), &mut out, progress)?;

    // A cover that was not asked for and one that no input carried come to the same thing here:
    // nothing beside the file, and nothing to say about it.
    let (cover_path, cover_error) = match (write_cover, &report.cover) {
        (true, Some(cover)) => match cover::write_beside(&taf_path, cover) {
            Ok(path) => (Some(path), None),
            Err(why) => (None, Some(why)),
        },
        _ => (None, None),
    };

    Ok(JobOutcome {
        taf_path,
        cover_path,
        cover_error,
        report,
    })
}

/// Why a job could not be run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobError {
    /// An input could not be opened.
    #[error("cannot open input {path}")]
    OpenInput {
        /// The file that was to be read, as the caller named it.
        path: PathBuf,
        /// What the filesystem said about it.
        #[source]
        source: std::io::Error,
    },
    /// The file the conversion writes into could not be created.
    #[error("cannot create output {path}")]
    CreateOutput {
        /// The file that was to be written, as the caller named it or as it was derived.
        path: PathBuf,
        /// What the filesystem said about it.
        #[source]
        source: std::io::Error,
    },
    /// The conversion itself failed.
    #[error(transparent)]
    Convert(#[from] ConvertError),
}

/// How long the file at `path` states it plays, without converting any of it.
///
/// This is the number a queue row shows and a percent bar counts against, read off the container's
/// headers: no audio is decoded, so a shelf of books costs file opens. What comes back is what the
/// file claims about itself — the conversion's own report stays the truth about the audio that was
/// written.
///
/// # Errors
///
/// [`ProbeError::Io`] if the file cannot be opened or stops being readable, and whatever
/// [`taf_encode::probe_duration()`] makes of its bytes otherwise: [`ProbeError::Unrecognized`] for
/// a file that is no container this build reads, [`ProbeError::NoDuration`] for one that states no
/// length.
pub fn probe_duration(path: &Path) -> Result<Duration, ProbeError> {
    let file = File::open(path)?;

    taf_encode::probe_duration(Box::new(file))
}

/// The chapters a Toniebox plays, as `FORMAT.md` states it from teddycloud's own limit.
///
/// A longer list is a warning and not a refusal: what a box does with the hundredth chapter is the
/// box's business, everything else that reads a TAF reads the whole of it, and a file that took an
/// hour to convert is not thrown away over a device that is not here.
pub const MAX_CHAPTERS: usize = 99;

/// How many chapters a stated plan comes to, where the caller stated one.
///
/// A TAF's first chapter begins where its audio does, so a plan that does not begin at the start of
/// it has that chapter put in front of it — one chapter more than was typed.
#[must_use]
pub fn planned_chapters(mode: &ChapterMode) -> Option<usize> {
    let ChapterMode::Explicit(offsets) = mode else {
        return None;
    };
    let opening = usize::from(offsets.first() != Some(&0));

    Some(offsets.len() + opening)
}

/// The inputs of `paths`, opened in the order they play.
///
/// What each of them is called is the path the caller stated, in full: it is what a failure of that
/// input renders as, and two directories can both hold a `01.mp3`.
fn opened(paths: &[PathBuf]) -> Result<Vec<Input>, JobError> {
    paths
        .iter()
        .map(|path| {
            let file = File::open(path).map_err(|source| JobError::OpenInput {
                path: path.clone(),
                source,
            })?;

            Ok(Input {
                reader: Box::new(file),
                name: path.display().to_string(),
            })
        })
        .collect()
}

/// The audio id a conversion is given: what time it is, in seconds since the Unix epoch.
///
/// The one clock read in taffle, and the reason the engine has none.
fn clock_audio_id() -> AudioId {
    // A clock set in front of the epoch is no time this can state, and 0 is the id of a file
    // converted at the beginning of it.
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());

    // The field is 32 bits wide and a timestamp is what goes in it, so the seconds go in as far as
    // they fit — which is what every 32-bit timestamp does, and what teddycloud writes there.
    #[allow(clippy::cast_possible_truncation)]
    let truncated = seconds as u32;

    AudioId::new(truncated)
}

#[cfg(test)]
mod tests {
    use super::{planned_chapters, ChapterMode};

    #[test]
    fn a_plan_that_skips_the_start_gains_the_opening_chapter() {
        assert_eq!(
            planned_chapters(&ChapterMode::Explicit(vec![0, 100])),
            Some(2)
        );
        assert_eq!(planned_chapters(&ChapterMode::Explicit(vec![100])), Some(2));
        assert_eq!(planned_chapters(&ChapterMode::Auto), None);
    }
}
