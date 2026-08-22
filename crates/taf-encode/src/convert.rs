//! The conversion itself: [`convert`] takes the inputs, the plan and the silence operations, and
//! writes the TAF they come to.
//!
//! # One pass, and what that decides
//!
//! Audio is read once. Nothing here rewinds an input, measures it and reads it again, because an
//! audiobook is hours long and a converter that reads it twice takes twice as long for an answer
//! it could have kept the first time. Everything below follows from that: how long an input is, is
//! not known until it has been read to the end, so nothing that has to be settled in front of the
//! audio may depend on it.
//!
//! # The three ways a plan comes about
//!
//! 1. **The caller states it.** [`ChapterMode::Explicit`] is what `--chapters` parsed to, and it
//!    overrides everything: neither the marks an input carries nor the boundaries between the
//!    inputs are consulted.
//! 2. **One input, and its own marks.** [`ChapterMode::Auto`] over a single input takes the marks
//!    that input carried — an m4b's chapter atom, and whatever else a container states.
//! 3. **More than one input, one chapter each.** [`ChapterMode::Auto`] over several inputs puts a
//!    chapter where each of them begins, and does not look at the marks they carry.
//!
//! # Why several files are the chapters and their own marks are not
//!
//! Somebody who hands a converter twelve files has stated what the chapters are by handing over
//! twelve files. Reading the marks inside them as well would put chapters where nobody asked for
//! any — and it could only ever be a mix of the two, since a set where one file carries marks and
//! eleven carry none is the ordinary case rather than the odd one. So the boundaries win whole:
//! that is one rule instead of a per-file lottery, and the way to have a file's own marks used is
//! to convert that file on its own or to state the plan outright.
//!
//! # What every plan holds
//!
//! A plan begins at offset 0, its offsets strictly increase, and every offset behind the first lies
//! in front of the end of the audio. The first two are what a chapter table *is* — a TAF's first
//! chapter begins where its audio does, and two chapters in one place are one chapter — and the
//! third is what makes every offset a place where there is something to play.
//!
//! What the stream then does with the plan is its own: the silence operations can move two chapters
//! onto the same frame by trimming everything between them away, and a file holds a block once, so
//! the chapters the file comes out with can be fewer than the plan had. A plan goes into the stream
//! strictly increasing all the same.
//!
//! # A mark is advisory, an offset the caller states is not
//!
//! An offset that a plan cannot hold — one at or behind the end of the audio, one that does not lie
//! behind the offset in front of it — is made to fit where it came out of a file and refused where
//! the caller stated it. What separates them is what the answer is worth to whoever gets it: a
//! container's chapter atom is not something its owner can correct, so a book whose marks are half
//! nonsense is still a book to convert, with the chapters of it that do make sense; an explicit
//! plan is what somebody typed a moment ago, so an offset in it that cannot be a chapter is a
//! mistake to state plainly rather than to quietly leave out.
//!
//! Making them fit is one pass over them: the marks are sorted into the order they play in, since
//! a container states them in whatever order it likes, and a mark where a chapter already begins is
//! that chapter rather than another, under the name the first mark there carried. A mark at or
//! behind the end of the audio needs no rule of its own — the stream ends in front of the block it
//! would have begun, so it never begins one.
//!
//! An explicit plan is held to both halves of that instead, and refused: offsets that do not
//! strictly increase are [`ChapterError::NotSorted`] before a byte is written, since nothing has to
//! be read to see it, and the first offset at or behind the end of the audio is
//! [`ChapterError::OutOfRange`] once the audio has run out, which is when that end is known.
//!
//! # The chapter a book has whatever is in it
//!
//! Offset 0 is in range even where there is no audio at all, because the first chapter of a TAF is
//! not something a plan chooses: it begins where the file's audio begins, and a file whose audio is
//! empty still has that one chapter. Every other offset has to be a place there is audio at — which
//! is also why an input of no length begins no chapter of its own in a set of them: the chapter it
//! would begin is the one the file behind it begins, and a file holds a block once.
//!
//! # The inputs are one stream, except where their boundaries are the chapters
//!
//! Where the boundaries are the chapters, those places are not known in front of the audio — so
//! each input is run as a stream of its own and its chapter is begun where the one in front of it
//! ended. The leading silence operations belong to the first of them; the per-chapter ones belong
//! to every one of them, which is what makes a file boundary a chapter start in every sense.
//!
//! Everything else — one input, or a plan the caller stated — is one stream over every input there
//! is, because a chapter offset is counted over the audio as a whole and the stage that moves those
//! offsets has to see it as a whole.
//!
//! # A chapter mark is placed while the stream runs
//!
//! The silence operations move chapter marks about, and where a mark ended up is settled the moment
//! its chapter begins — which
//! [`SilenceProcessor::chapters_emitted`](crate::SilenceProcessor::chapters_emitted) states as the
//! stream runs. A block never spans a chapter start, so a mark that has appeared belongs in front of
//! the block in hand: the frame being encoded is filled out, the chapter is begun, and the block
//! goes on into the frame behind it.
//!
//! # Where a title belongs
//!
//! To the mark it was authored on, and to nothing else. The plan a conversion runs is not the list
//! of marks an input carried: it begins at offset 0 whether a mark did or not, marks that cannot be
//! chapters are left out of it, and marks out of order are sorted into it. So a title travels with
//! its offset from the moment the mark is read, and the chapter that has it is the chapter that
//! came from that mark — the one at offset 0 having no title unless a mark began there, and a file
//! boundary or an offset the caller typed having none at all.

use core::fmt;
use std::io::{Seek, Write};
use std::time::Duration;

use symphonia::core::io::MediaSource;
use taf::id::{AudioId, BlockIndex};
use taf::ogg::OPUS_PRE_SKIP;
use taf::writer::{WriterError, WriterIoError};

use crate::chapters::{ChapterError, ChapterMode};
use crate::decode::Cover;
use crate::encode::TafEncoder;
use crate::pcm::{PcmError, SilenceOpts};
use crate::produce::{produce, Feed, Produced};

/// The rate everything behind the sample stage is counted at, and the one a TAF carries.
pub(crate) const RATE: u32 = 48_000;

/// The channels every block behind it interleaves.
pub(crate) const CHANNELS: u16 = 2;

/// How many blocks the producer may run ahead: a block is a few thousand samples, so this is
/// well under a second of audio and a bounded amount of memory.
const FEED_DEPTH: usize = 64;

/// One input of a conversion.
///
/// The name is what a failure calls it and nothing else: no format is decided by it, and an input
/// that came from no file at all can be called whatever the caller likes.
pub struct Input {
    /// Where the input's bytes come from.
    pub reader: Box<dyn MediaSource>,
    /// What to call the input when something about it fails.
    pub name: String,
}

impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Input")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// What a conversion is asked to do with the inputs it is handed.
///
/// The audio id is not here: it is the one thing a conversion cannot decide for itself without
/// reading a clock, so it is [`convert`]'s own parameter and the engine stays clock-free.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Conversion {
    /// How the chapters of the output are decided.
    pub chapter_mode: ChapterMode,
    /// What is taken off the audio and what is put into it.
    pub silence: SilenceOpts,
}

/// What a conversion is doing, as it does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Progress {
    /// The conversion has reached an input and is reading it, counted over all of them.
    Decoding {
        /// Which input, counted from the first one handed in.
        input_index: usize,
    },
    /// The audio encoded so far, in frames of one channel at 48 kHz. Only ever grows.
    Encoded {
        /// How many of those frames have gone into the file.
        samples_done: u64,
    },
    /// The audio is all in, and the file is being closed.
    Finalizing,
}

/// One chapter of the file a conversion wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterOut {
    /// The block of the audio region the chapter starts at, which is what a box seeks on.
    pub page: BlockIndex,
    /// How far into the audio it starts.
    pub start: Duration,
    /// What the input called it, where anything did.
    pub title: Option<String>,
}

/// What a conversion came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionReport {
    /// The chapters the file holds, in the order it holds them — one per block a chapter starts
    /// at, which is not always one per chapter that was planned. See [`convert`].
    pub chapters: Vec<ChapterOut>,
    /// How long the file plays.
    pub duration: Duration,
    /// The cover art of the first input that carried any.
    pub cover: Option<Cover>,
    /// The audio id the file was written with, as it was handed in.
    pub audio_id: AudioId,
}

/// Converts `inputs` into the TAF `out`, and states what it came to.
///
/// The inputs are decoded, brought to the 48 kHz stereo a TAF carries, put through the silence
/// operations `opts` states, and encoded into Opus packets that go into the file with the chapter
/// marks the plan came to. `audio_id` is the caller's — a Toniebox reads it, and deriving it from a
/// clock is the caller's business, not an engine's. `progress` is called as the conversion runs;
/// nothing about the file depends on what it does.
///
/// # Chapters that come out in one place
///
/// A chapter whose audio the silence operations trimmed away entirely begins where the chapter
/// behind it begins, and a TAF holds a block once. The file then holds one chapter there, and so
/// does the report: the one that is played there, which is the last of those that landed on it,
/// under its own name. So the report has one entry per chapter the *file* has, which can be fewer
/// than the plan had.
///
/// # An explicit offset behind the end of the audio
///
/// How much audio a conversion comes to is known when the last frame of it is out and not before,
/// so an offset of [`ChapterMode::Explicit`] that lies at or behind that end is refused *there*:
/// the conversion fails with [`ChapterError::OutOfRange`] stating the offset and the length that
/// was found, and `out` is left holding a file that was never finished — its header block is the
/// zeros that were reserved for it. Offsets that do not strictly increase need no length to be
/// refused, and are refused before anything is written at all.
///
/// # The audio is read ahead of being encoded
///
/// The inputs are read and decoded on a thread of their own, which hands the blocks over a bounded
/// channel, so that reading the input behind one overlaps encoding the one in hand. Nothing about
/// the file follows from that: the encoder is handed the same blocks in the same order it always
/// was, on this thread, and `progress` is called from here as well — so `out` and the callback see
/// one thread, and the file is the file it was.
///
/// # Errors
///
/// - [`ConvertError::Chapters`] if there are no inputs, or the offsets stated are no plan.
/// - [`ConvertError::Input`] if an input could not be read, decoded, or brought to 48 kHz stereo.
/// - [`ConvertError::Encode`] if libopus refused a frame.
/// - [`ConvertError::Taf`] or [`ConvertError::Io`] if the file could not be written.
pub fn convert<W: Write + Seek>(
    inputs: Vec<Input>,
    opts: &Conversion,
    audio_id: AudioId,
    out: W,
    progress: &mut dyn FnMut(Progress),
) -> Result<ConversionReport, ConvertError> {
    if inputs.is_empty() {
        return Err(ChapterError::Empty.into());
    }
    if let ChapterMode::Explicit(offsets) = &opts.chapter_mode {
        increasing(offsets)?;
    }

    let mut encoder = TafEncoder::new(audio_id, out)?;
    let mut chapters: Vec<ChapterOut> = Vec::new();

    let produced = std::thread::scope(|scope| -> Result<Produced, ConvertError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(FEED_DEPTH);
        let decoding = scope.spawn(move || produce(inputs, opts, &tx));

        let mut outcome = Ok(());
        for feed in &rx {
            let fed = match feed {
                Feed::Reached(input_index) => {
                    progress(Progress::Decoding { input_index });
                    Ok(())
                }
                Feed::Chapter(title) => encoder.begin_chapter().map(|()| {
                    place(
                        &mut chapters,
                        ChapterOut {
                            page: encoder.block(),
                            start: position(encoder.frames()),
                            title,
                        },
                    );
                }),
                Feed::Block(block) => encoder.push(&block).map(|()| {
                    progress(Progress::Encoded {
                        samples_done: encoder.frames(),
                    });
                }),
            };
            if let Err(failure) = fed {
                outcome = Err(failure);
                break;
            }
        }
        // Dropping the receiver is what stops a producer whose consumer failed.
        drop(rx);

        let produced = decoding
            .join()
            .map_err(|_| ConvertError::Io(std::io::Error::other("the decoding thread failed")))?;
        outcome?;

        produced
    })?;

    if let ChapterMode::Explicit(offsets) = &opts.chapter_mode {
        within(offsets, produced.frames)?;
    }

    progress(Progress::Finalizing);
    let frames = encoder.finish()?;

    // A file whose audio came to nothing still begins the chapter every TAF begins at block 0.
    if chapters.is_empty() {
        chapters.push(ChapterOut {
            page: BlockIndex::new(0),
            start: Duration::ZERO,
            title: produced.opening_title,
        });
    }

    Ok(ConversionReport {
        chapters,
        duration: playtime(frames),
        cover: produced.cover,
        audio_id,
    })
}

/// Why a conversion could not be made.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConvertError {
    /// An input could not be read, decoded, or brought to the shape a TAF carries.
    #[error("input '{name}' failed")]
    Input {
        /// What the conversion was told to call the input.
        name: String,
        /// What went wrong with it.
        #[source]
        source: PcmError,
    },
    /// The chapters the conversion was asked for are no plan.
    #[error(transparent)]
    Chapters(#[from] ChapterError),
    /// The Opus encoder refused a frame, or the settings it was built with.
    #[error("opus encoding failed")]
    Encode(#[from] opus::Error),
    /// The file could not be written the way a TAF is laid out.
    #[error("taf writing failed")]
    Taf(#[source] WriterError),
    /// Writing to the output itself failed.
    #[error("output i/o failed")]
    Io(#[from] std::io::Error),
}

impl From<WriterIoError> for ConvertError {
    fn from(error: WriterIoError) -> Self {
        match error {
            WriterIoError::Writer(error) => Self::Taf(error),
            WriterIoError::Io(error) => Self::Io(error),
            // The writer states those two and may come to state more; whatever a later one is, it
            // happened to the file this was writing.
            other => Self::Io(std::io::Error::other(other)),
        }
    }
}

/// Whether the offsets the caller stated could be a plan at all: the half of the check that needs
/// no length, and so the half that is answered before anything is written.
fn increasing(offsets: &[u64]) -> Result<(), ChapterError> {
    let mut previous = None;
    for offset in offsets.iter().copied() {
        if previous.is_some_and(|earlier| offset <= earlier) {
            return Err(ChapterError::NotSorted);
        }
        previous = Some(offset);
    }

    Ok(())
}

/// Whether every offset the caller stated is a place the audio reached, which is the other half —
/// and the half nothing can answer before the last frame is out.
fn within(offsets: &[u64], total: u64) -> Result<(), ChapterError> {
    // Offset 0 is in range whatever the length is: the first chapter of a TAF begins where its
    // audio does, and a book with no audio in it still has that chapter.
    match offsets
        .iter()
        .copied()
        .find(|offset| *offset >= total && *offset != 0)
    {
        Some(offset) => Err(ChapterError::OutOfRange { offset, total }),
        None => Ok(()),
    }
}

/// Puts a chapter in the report, or in the place of the one it came out on top of.
///
/// Two chapters at one block are one chapter in the file, and the one played there is the later of
/// them: a chapter that came out where the chapter behind it begins had its audio trimmed away to
/// nothing, so what is played at that block is the audio of the one behind it, under its name.
fn place(chapters: &mut Vec<ChapterOut>, chapter: ChapterOut) {
    match chapters.last_mut() {
        Some(last) if last.page == chapter.page => *last = chapter,
        _ => chapters.push(chapter),
    }
}

/// How far into the audio `frames` frames at 48 kHz lie.
///
/// Whole seconds and the nanoseconds left over, so that nothing is lost to a float on the way: one
/// frame is 20 833 and a third nanoseconds, and what the division leaves of that is under a
/// nanosecond of the answer.
fn position(frames: u64) -> Duration {
    let rate = u64::from(RATE);
    let rest = (frames % rate) * 1_000_000_000 / rate;

    Duration::new(frames / rate, u32::try_from(rest).unwrap_or(0))
}

/// How long a file of `frames` frames plays: what it carries, less the samples a player drops in
/// front of the audio.
fn playtime(frames: u64) -> Duration {
    position(frames.saturating_sub(u64::from(OPUS_PRE_SKIP)))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{ConvertError, Input, Progress, WriterError, WriterIoError};
    use std::io::{self, Cursor};

    #[test]
    fn an_input_shows_what_it_is_called_and_not_the_bytes_behind_it() {
        let input = Input {
            reader: Box::new(Cursor::new(Vec::new())),
            name: String::from("book.m4b"),
        };

        assert_eq!(format!("{input:?}"), "Input { name: \"book.m4b\", .. }");
    }

    #[test]
    fn the_file_failing_and_the_format_failing_are_different_refusals() {
        // Which is the whole of what the writer's two failures are told apart for: a caller
        // rendering an error chain says "there is no room on the disk" or "this cannot be a TAF",
        // and never one of them for the other.
        let format = ConvertError::from(WriterIoError::Writer(WriterError::PacketTooLarge));
        let file = ConvertError::from(WriterIoError::Io(io::Error::other("no room left")));

        assert!(matches!(format, ConvertError::Taf(_)), "{format:?}");
        assert_eq!(format.to_string(), "taf writing failed");
        assert!(matches!(file, ConvertError::Io(_)), "{file:?}");
        assert_eq!(file.to_string(), "output i/o failed");
    }

    #[test]
    fn what_a_conversion_is_doing_says_which_input_and_how_far_in() {
        // The progress a caller matches on, which is the whole of what it can be told.
        assert_eq!(
            format!("{:?}", Progress::Decoding { input_index: 2 }),
            "Decoding { input_index: 2 }"
        );
        assert_eq!(
            Progress::Encoded {
                samples_done: 2_880
            },
            Progress::Encoded {
                samples_done: 2_880
            }
        );
        assert_ne!(Progress::Finalizing, Progress::Decoding { input_index: 0 });
    }
}
