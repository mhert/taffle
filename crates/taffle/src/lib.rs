//! File-oriented conversion workflows shared by any frontend (CLI today, GUI anticipated).
//!
//! The engine below this takes readers and hands back a report; a person converting an audiobook
//! has files, and wants the one that comes out named after the one that went in with its cover
//! beside it. That gap is the whole of this crate: paths in, [`taf_encode::convert()`] in the
//! middle, a `.taf` and a picture out. Nothing here decodes, encodes or decides anything about the
//! audio.
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

mod cover;
mod output;

use std::fs::File;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use taf::id::AudioId;
use taf_encode::{
    convert, ChapterError, Conversion, ConversionReport, ConvertError, Input, Progress,
};

pub use output::default_output_path;

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
/// through as it comes.
///
/// # Errors
///
/// - [`JobError::OpenInput`] if an input could not be opened.
/// - [`JobError::CreateOutput`] if the TAF could not be created.
/// - [`JobError::Convert`] if the conversion itself failed. A job of no inputs is one of those,
///   refused as [`ChapterError::Empty`] before a file is made for it: what the engine calls having
///   nothing to convert is what a caller is handed here, rather than a second name for it.
///
/// A conversion that failed part-way leaves the output file behind holding what was written before
/// it failed, which is [`taf_encode::convert()`]'s business and stated there.
///
/// Failing to write the cover is *not* one of these; it comes back in
/// [`JobOutcome::cover_error`].
pub fn run_convert(
    job: ConvertJob,
    progress: &mut dyn FnMut(Progress),
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
    let report = convert(sources, &options, clock_audio_id(), out, progress)?;

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
