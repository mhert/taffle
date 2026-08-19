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
//! # The inputs are one stream, except where their boundaries are the chapters
//!
//! [`ChapterMode::Auto`] over several inputs puts a chapter where each of them begins, and those
//! places are not known in front of the audio — so each input is run as a stream of its own and its
//! chapter is begun where the one in front of it ended. The leading silence operations belong to
//! the first of them; the per-chapter ones belong to every one of them, which is what makes a file
//! boundary a chapter start in every sense.
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
use std::cell::{Cell, RefCell};
use std::io::{Seek, Write};
use std::rc::Rc;
use std::time::Duration;
use std::vec::IntoIter;

use symphonia::core::io::MediaSource;
use taf::id::{AudioId, BlockIndex};
use taf::ogg::OPUS_PRE_SKIP;
use taf::writer::{WriterError, WriterIoError};

use crate::chapters::{ChapterError, ChapterMode};
use crate::decode::{open_source, AudioSource, Cover, DecodeError, SourceMetadata, SourceSpec};
use crate::encode::TafEncoder;
use crate::pcm::{Pcm48, PcmError, SilenceOpts, SilenceProcessor};

/// The rate everything behind the sample stage is counted at, and the one a TAF carries.
const RATE: u32 = 48_000;

/// The channels every block behind it interleaves.
const CHANNELS: u16 = 2;

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

    let names: Vec<String> = inputs.iter().map(|input| input.name.clone()).collect();
    let reading = Rc::new(Reading::default());
    // What a failure of the stream says: the input it happened in, and the failure the
    // concatenation kept where the trait it hands blocks over had no room for it.
    let failed = |source: PcmError| ConvertError::Input {
        name: names.get(reading.at.get()).cloned().unwrap_or_default(),
        source: reading.kept().unwrap_or(source),
    };

    let mut encoder = TafEncoder::new(audio_id, out)?;
    let mut chapters: Vec<ChapterOut> = Vec::new();
    let mut opening_title = None;
    let mut reported = 0;

    let streams = streamed(inputs, &opts.chapter_mode);
    let per_input = streams.len() > 1;

    for (base, inputs) in streams {
        let mut concat = Concat::new(inputs, base, Rc::clone(&reading));
        let marks = concat.prime().map_err(&failed)?;
        reached(&reading, &mut reported, progress);

        let plan = match (&opts.chapter_mode, per_input) {
            (ChapterMode::Explicit(offsets), _) => stated(offsets),
            // One input, and the marks it carried are the chapters it has.
            (ChapterMode::Auto, false) => authored(marks),
            // One of several, which is one chapter beginning where it does.
            (ChapterMode::Auto, true) => vec![Chapter::opening()],
        };
        if base == 0 {
            opening_title = plan.first().and_then(|chapter| chapter.title.clone());
        }

        let offsets = plan.iter().map(|chapter| chapter.offset).collect();
        let pcm = Pcm48::new(Box::new(concat)).map_err(&failed)?;
        let mut stream = SilenceProcessor::new(pcm, offsets, silence(&opts.silence, base));
        let mut begun = 0;

        while let Some(block) = stream.next_block().map_err(&failed)? {
            reached(&reading, &mut reported, progress);

            // Every chapter whose place is settled begins in front of this block: the frame in
            // hand is filled out and the chapter starts the block behind it.
            let settled = stream.chapters_emitted().len();
            for at in begun..settled {
                encoder.begin_chapter()?;
                place(
                    &mut chapters,
                    ChapterOut {
                        page: encoder.block(),
                        start: position(encoder.frames()),
                        title: plan.get(at).and_then(|chapter| chapter.title.clone()),
                    },
                );
            }
            begun = settled;

            encoder.push(&block)?;
            progress(Progress::Encoded {
                samples_done: encoder.frames(),
            });
        }

        // An input that handed out no blocks at all was still reached.
        reached(&reading, &mut reported, progress);
    }

    if let ChapterMode::Explicit(offsets) = &opts.chapter_mode {
        within(offsets, reading.frames.get())?;
    }

    progress(Progress::Finalizing);
    let frames = encoder.finish()?;

    // A file whose audio came to nothing still begins the chapter every TAF begins at block 0.
    if chapters.is_empty() {
        chapters.push(ChapterOut {
            page: BlockIndex::new(0),
            start: Duration::ZERO,
            title: opening_title,
        });
    }

    Ok(ConversionReport {
        chapters,
        duration: playtime(frames),
        cover: reading.cover.take(),
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

/// Where a chapter begins and what it is called: a mark an input carried, and an entry of the plan
/// a conversion runs — which are the same thing, since a plan is what became of the marks.
///
/// Offsets are frames at 48 kHz, counted from the start of the stream the chapter belongs to.
struct Chapter {
    offset: u64,
    title: Option<String>,
}

impl Chapter {
    /// The chapter every plan begins with, which no mark named.
    fn opening() -> Self {
        Self {
            offset: 0,
            title: None,
        }
    }
}

/// What the concatenation under a conversion tells it while it runs.
///
/// The stream a conversion pulls from is inside the silence processing by the time it runs, so what
/// happens down there is left here on the way past: which input is being read, how much they have
/// decoded to, the cover art the first one to carry any carried — and the one failure a source
/// cannot state in the error type its trait hands back.
#[derive(Default)]
struct Reading {
    /// The input being read, counted over the whole conversion.
    at: Cell<usize>,
    /// The frames of 48 kHz stereo the inputs have decoded to, which is what an offset the caller
    /// states is counted in.
    frames: Cell<u64>,
    /// The cover art of the first input that carried any.
    cover: RefCell<Option<Cover>>,
    /// A failure of the sample stage, kept where [`DecodeError`] has no room for it.
    failure: RefCell<Option<PcmError>>,
}

impl Reading {
    /// The failure that was kept where it could not be stated, if one was.
    fn kept(&self) -> Option<PcmError> {
        self.failure.borrow_mut().take()
    }

    /// Keeps `cover` where nothing has been kept yet: the cover a conversion has is the one the
    /// first input to carry any carried.
    fn carry(&self, cover: Option<Cover>) {
        let mut carried = self.cover.borrow_mut();

        if carried.is_none() {
            *carried = cover;
        }
    }
}

/// The inputs of a conversion as one stream: each of them decoded and brought to 48 kHz stereo on
/// its own, one after the other.
///
/// Which makes the concatenation an [`AudioSource`] of exactly the shape the sample stage brings
/// everything to, so the [`Pcm48`] over it hands its blocks through untouched — and a whole
/// conversion, several files and all, goes through one [`SilenceProcessor`]. Counting the frames
/// here is the same count [`Pcm48::scale_samples`] answers in, so a chapter offset and the length
/// it is held against are in one unit without anything being scaled twice.
struct Concat {
    /// The inputs still to be opened, in the order they play.
    pending: IntoIter<Input>,
    /// The one being read.
    current: Option<Pcm48>,
    /// How many of them have been opened.
    opened: usize,
    /// Where the first of them stands in the conversion, so that what is reported about an input
    /// is counted over every input there is and not over this stream's.
    base: usize,
    /// What the conversion around this is told while it runs.
    reading: Rc<Reading>,
}

impl Concat {
    /// A stream over `inputs`, the first of which is input `base` of the conversion.
    fn new(inputs: Vec<Input>, base: usize, reading: Rc<Reading>) -> Self {
        Self {
            pending: inputs.into_iter(),
            current: None,
            opened: 0,
            base,
            reading,
        }
    }

    /// Opens the first input and hands over the chapter marks it carried.
    ///
    /// Opening it here rather than at the first block is what lets those marks be the plan the
    /// conversion runs: they have to be in front of the stream that places them.
    fn prime(&mut self) -> Result<Vec<Chapter>, PcmError> {
        Ok(self.open()?.unwrap_or_default())
    }

    /// Opens the next input, if there is one, and hands over the marks it carried.
    fn open(&mut self) -> Result<Option<Vec<Chapter>>, DecodeError> {
        let Some(input) = self.pending.next() else {
            return Ok(None);
        };
        self.reading.at.set(self.base + self.opened);
        self.opened += 1;

        let mut source = open_source(input.reader)?;
        // Everything the container says about the recording is read in front of the stage that
        // consumes the source, and the marks are scaled by the stage that knows what it did to the
        // samples around them.
        let metadata = source.metadata();
        let pcm = Pcm48::new(source).map_err(|failure| self.keep(failure))?;
        let marks = metadata
            .chapters
            .into_iter()
            .map(|mark| Chapter {
                offset: pcm.scale_samples(mark.start_sample),
                title: mark.title,
            })
            .collect();

        self.reading.carry(metadata.cover);
        self.current = Some(pcm);

        Ok(Some(marks))
    }

    /// Keeps a failure of the sample stage where the conversion picks it up, and states what a
    /// source's own error type can say about it.
    ///
    /// A source states its failures as [`DecodeError`], which has no shape for the sample stage's
    /// own — so one of those is left where the conversion takes it, the way `taf`'s writer leaves
    /// the io error a page sink could not report. A decode failure is the one shape both types
    /// hold, and travels as itself.
    fn keep(&self, failure: PcmError) -> DecodeError {
        match failure {
            PcmError::Decode(failure) => failure,
            failure => {
                self.reading.failure.replace(Some(failure));

                DecodeError::UnsupportedFormat
            }
        }
    }
}

impl AudioSource for Concat {
    fn spec(&self) -> SourceSpec {
        SourceSpec {
            sample_rate: RATE,
            channels: CHANNELS,
        }
    }

    /// Nothing of its own: what each input carried was read as that input was opened.
    fn metadata(&mut self) -> SourceMetadata {
        SourceMetadata::default()
    }

    fn next_block(&mut self) -> Result<Option<Vec<i16>>, DecodeError> {
        loop {
            let Some(pcm) = self.current.as_mut() else {
                // The next input, and the end of the stream where there is none left to open.
                // What that input carried is the plan's business, and a plan is settled in front
                // of the stream it is placed in.
                let _ = self.open()?;
                if self.current.is_none() {
                    return Ok(None);
                }

                continue;
            };

            match pcm.next_block() {
                Ok(Some(block)) => {
                    let frames =
                        u64::try_from(block.len() / usize::from(CHANNELS)).unwrap_or(u64::MAX);
                    self.reading
                        .frames
                        .set(self.reading.frames.get().saturating_add(frames));

                    return Ok(Some(block));
                }
                // The end of this input, which is where the next one begins.
                Ok(None) => self.current = None,
                Err(failure) => return Err(self.keep(failure)),
            }
        }
    }
}

/// How the inputs are streamed: one stream per input where the boundaries between them are the
/// chapters, and one stream over all of them otherwise.
fn streamed(inputs: Vec<Input>, mode: &ChapterMode) -> Vec<(usize, Vec<Input>)> {
    if matches!(mode, ChapterMode::Auto) && inputs.len() > 1 {
        return inputs
            .into_iter()
            .enumerate()
            .map(|(at, input)| (at, vec![input]))
            .collect();
    }

    vec![(0, inputs)]
}

/// What the silence operations are for the stream beginning at input `base`.
///
/// The leading ones are the conversion's own and belong to the audio's first frame, so a stream
/// that does not begin there is handed the per-chapter ones only — which are what a file boundary
/// gets, since a boundary is where a chapter begins.
fn silence(opts: &SilenceOpts, base: usize) -> SilenceOpts {
    if base == 0 {
        return *opts;
    }

    SilenceOpts {
        skip_leading: 0,
        trim_leading: false,
        add_pause_leading: 0,
        ..*opts
    }
}

/// The plan the marks an input carried make.
///
/// Every plan begins at offset 0, since the first chapter of a TAF begins where its audio does, and
/// its offsets strictly increase: a mark where a chapter already begins is that chapter rather than
/// another, name and all, and one in front of the chapter already planned is no chapter of its own.
/// Which is [`resolve_chapters`](crate::resolve_chapters)' rule for marks, with the one half of it
/// that needs a length left to the stream: a mark behind the end of the audio never begins a
/// chapter there, and nothing knows where that end is until the audio has run out.
///
/// Marks come out of a container in the order the container states them, which is not necessarily
/// the order they play in.
fn authored(mut marks: Vec<Chapter>) -> Vec<Chapter> {
    marks.sort_by_key(|mark| mark.offset);

    let mut plan: Vec<Chapter> = Vec::with_capacity(marks.len() + 1);
    for mark in marks {
        match plan.last() {
            // Behind the last chapter planned, so a chapter of its own.
            Some(last) if mark.offset > last.offset => plan.push(mark),
            // Where one already begins, which is that same chapter.
            Some(_) => {}
            // The first of them, which is the chapter the file opens with where it begins at the
            // start of the audio — and the one behind it where nothing was marked there.
            None if mark.offset == 0 => plan.push(mark),
            None => plan.extend([Chapter::opening(), mark]),
        }
    }
    if plan.is_empty() {
        plan.push(Chapter::opening());
    }

    plan
}

/// The plan the caller stated.
///
/// No chapter of one is named: an offset somebody typed is a place, and what an input happened to
/// call a mark near it is not that place's name.
fn stated(offsets: &[u64]) -> Vec<Chapter> {
    let mut plan = Vec::with_capacity(offsets.len() + 1);
    if offsets.first() != Some(&0) {
        plan.push(Chapter::opening());
    }
    plan.extend(offsets.iter().map(|offset| Chapter {
        offset: *offset,
        title: None,
    }));

    plan
}

/// Whether the offsets the caller stated could be a plan at all: the half of
/// [`resolve_chapters`](crate::resolve_chapters)' check that needs no length.
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

/// Reports every input the conversion has reached since it last did.
///
/// An input is reached once, in the order they play: the concatenation states which one it is
/// reading, and everything from the last one reported to that one is announced here.
fn reached(reading: &Reading, reported: &mut usize, progress: &mut dyn FnMut(Progress)) {
    let at = reading.at.get();

    for input_index in *reported..=at {
        progress(Progress::Decoding { input_index });
    }
    *reported = at.saturating_add(1);
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
    use super::{
        AudioSource, Concat, ConvertError, Input, Progress, Reading, SourceMetadata, WriterError,
        WriterIoError,
    };
    use std::io::{self, Cursor};
    use std::rc::Rc;

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

    #[test]
    fn nothing_is_kept_where_nothing_went_wrong() {
        let reading = Reading::default();

        assert!(reading.kept().is_none());
    }

    #[test]
    fn a_concatenation_carries_no_metadata_of_its_own() {
        // What the inputs carried was read as each of them was opened, in front of the stage that
        // consumed the source — so there is nothing left here to answer with.
        let mut concat = Concat::new(Vec::new(), 0, Rc::new(Reading::default()));

        assert_eq!(concat.metadata(), SourceMetadata::default());
    }
}
