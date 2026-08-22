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
use crate::chunk::{Chunker, Job};
use crate::decode::Cover;
use crate::encode::{encode_job, PacketSink, FRAME_SAMPLES};
use crate::pcm::{PcmError, SilenceOpts};
use crate::produce::{produce, Feed, Produced};

/// The rate everything behind the sample stage is counted at, and the one a TAF carries.
pub(crate) const RATE: u32 = 48_000;

/// How many blocks the producer may run ahead: a block is a few thousand frames, so this is a few
/// seconds of audio and well under a megabyte held at once.
const FEED_DEPTH: usize = 64;

/// How many jobs may wait in front of the workers. With the jobs a pool is chewing on, this
/// bounds the PCM in flight to a few chunks' worth of memory.
const JOB_DEPTH: usize = 2;

/// What a worker hands back: which job, and what it encoded to.
type Batch = (usize, Result<Vec<Vec<u8>>, opus::Error>);

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
    /// How many encoders run at once. `None` is one per core the machine states. What it never
    /// changes is the file: the bytes are the same whatever number runs.
    pub workers: Option<std::num::NonZeroUsize>,
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
/// # The audio is read ahead of being encoded, and encoded on every core
///
/// The inputs are read and decoded on a thread of their own, which hands the blocks over a bounded
/// channel, so that reading the input behind one overlaps encoding the one in hand. Here the blocks
/// are cut into chunks and the chunks are handed to a pool of as many encoders as
/// [`Conversion::workers`] asks for.
///
/// Nothing about the file follows from any of that: where a chunk is cut is a function of the audio
/// alone, a chunk encodes to the same packets whichever worker takes it, and the packets go into
/// the file in the order the chunks were cut, however the workers finished. Writing and `progress`
/// both happen on this thread — so `out` and the callback see one thread, and the file is the
/// audio's rather than the machine's.
///
/// # Errors
///
/// - [`ConvertError::Chapters`] if there are no inputs, or the offsets stated are no plan.
/// - [`ConvertError::Input`] if an input could not be read, decoded, or brought to 48 kHz stereo.
/// - [`ConvertError::Encode`] if libopus refused a frame.
/// - [`ConvertError::Taf`] or [`ConvertError::Io`] if the file could not be written.
/// - [`ConvertError::Io`] as well if the thread reading the inputs or one of the threads encoding
///   them failed outright, since a thread that is gone leaves no failure of its own behind.
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

    let workers = opts.workers.map_or_else(
        || std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        std::num::NonZeroUsize::get,
    );

    let mut sink = PacketSink::new(audio_id, out)?;
    let mut chapters: Vec<ChapterOut> = Vec::new();

    let produced = std::thread::scope(|scope| -> Result<Produced, ConvertError> {
        let (feed_tx, feed_rx) = std::sync::mpsc::sync_channel(FEED_DEPTH);
        let decoding = scope.spawn(move || produce(inputs, opts, &feed_tx));

        let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<Job>(JOB_DEPTH);
        // The results channel is unbounded on purpose: what bounds the memory is how many jobs
        // exist at once, and that is the job channel plus the workers holding one each. A bounded
        // results channel could instead deadlock — every worker blocked handing back, the calling
        // thread blocked handing out.
        let (batch_tx, batch_rx) = std::sync::mpsc::channel::<Batch>();
        // The queue belongs to the workers and to nobody else, so a pool that died to the last of
        // them takes it with it — which is what makes handing a job over a failure to state rather
        // than a wait for somebody who is never coming.
        let queue = std::sync::Arc::new(std::sync::Mutex::new(job_rx));
        for _ in 0..workers {
            let batch_tx = batch_tx.clone();
            let queue = std::sync::Arc::clone(&queue);
            scope.spawn(move || {
                // A job is taken under the lock and encoded outside of it, so the workers share a
                // queue and never an encoder. What a batch is worth once the calling thread has
                // given up is nothing, which is why a send that finds nobody there is no news.
                while let Some(job) = next_job(&queue) {
                    let index = job.index;
                    let _ = batch_tx.send((index, encode_job(&job)));
                }
            });
        }
        drop(batch_tx);
        drop(queue);

        let mut collector = Collector::new(&mut sink, &mut chapters, progress);
        // The feeding takes the feeds over and lets go of them when it is done, which is what
        // stops a producer whose consumer failed — at a failure and at the end of the audio alike.
        let fed = feeding(&mut collector, feed_rx, &job_tx, &batch_rx);
        // And letting go of the queue is what tells the workers nothing further is coming.
        drop(job_tx);
        let outcome = fed.and_then(|()| collector.drain(&batch_rx));

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
    // A TAF's first block holds an audio page, so a conversion that came out with no audio still
    // writes the one 60 ms frame of silence that makes it a file.
    if sink.frames() == 0 {
        let silence = encode_job(&Job {
            index: 0,
            warmup: Vec::new(),
            pcm: vec![0; FRAME_SAMPLES],
            chapters: Vec::new(),
        })?;
        for packet in &silence {
            sink.push_packet(packet)?;
        }
    }
    let frames = sink.finish()?;

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
    /// Writing to the output itself failed, or a thread of the conversion did.
    ///
    /// A thread that fails outright leaves no failure of its own behind to state, so the reading
    /// thread or an encoding worker going missing is stated here as well — which is the file being
    /// written failing either way, and the error inside says which half it was.
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

/// The next job of the queue the workers share, waited for under the lock so that one job goes to
/// one worker. [`None`] is a queue that is closed and empty, which is the end of the audio.
///
/// A poisoned lock is a worker that failed while holding it; the queue behind it is a channel that
/// no failure could have left half-read, so the jobs still in it are jobs to encode.
fn next_job(jobs: &std::sync::Mutex<std::sync::mpsc::Receiver<Job>>) -> Option<Job> {
    let queue = jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    queue.recv().ok()
}

/// Cuts what the reading hands over into chunks, hands every chunk to the pool, and writes the
/// batches that come back on the way.
///
/// # Errors
///
/// What encoding a chunk failed with, or what writing one to the file failed with.
fn feeding<W: Write + Seek>(
    collector: &mut Collector<'_, W>,
    feeds: std::sync::mpsc::Receiver<Feed>,
    jobs: &std::sync::mpsc::SyncSender<Job>,
    batches: &std::sync::mpsc::Receiver<Batch>,
) -> Result<(), ConvertError> {
    let mut chunker = Chunker::new();

    for feed in feeds {
        let job = match feed {
            Feed::Reached(input_index) => {
                collector.reached(input_index);
                None
            }
            Feed::Chapter(title) => chunker.begin_chapter(title),
            Feed::Block(block) => chunker.push_block(block),
        };
        if let Some(job) = job {
            collector.dispatch(job, jobs, batches)?;
        }
    }
    if let Some(job) = chunker.finish() {
        collector.dispatch(job, jobs, batches)?;
    }

    Ok(())
}

/// The writing end of the pool: batches in job order, chapters where their chunks begin, and
/// the progress the caller watches.
struct Collector<'a, W: Write + Seek> {
    sink: &'a mut PacketSink<W>,
    chapters: &'a mut Vec<ChapterOut>,
    progress: &'a mut dyn FnMut(Progress),
    /// Batches that arrived ahead of their turn, by job index.
    pending: std::collections::BTreeMap<usize, Result<Vec<Vec<u8>>, opus::Error>>,
    /// The chapters of jobs not yet written, by job index.
    marks: std::collections::BTreeMap<usize, Vec<Option<String>>>,
    /// How many jobs went out.
    dispatched: usize,
    /// The job whose batch is written next.
    next: usize,
}

impl<'a, W: Write + Seek> Collector<'a, W> {
    /// A collector at the start of a conversion, with nothing out and nothing waiting.
    fn new(
        sink: &'a mut PacketSink<W>,
        chapters: &'a mut Vec<ChapterOut>,
        progress: &'a mut dyn FnMut(Progress),
    ) -> Self {
        Self {
            sink,
            chapters,
            progress,
            pending: std::collections::BTreeMap::new(),
            marks: std::collections::BTreeMap::new(),
            dispatched: 0,
            next: 0,
        }
    }

    /// The conversion has reached an input, which is the caller's to hear about.
    fn reached(&mut self, input_index: usize) {
        (self.progress)(Progress::Decoding { input_index });
    }

    /// Hands a job to the pool and writes whatever batches are already waiting.
    ///
    /// # Errors
    ///
    /// What writing a waiting batch failed with, or that the pool is gone.
    fn dispatch(
        &mut self,
        job: Job,
        jobs: &std::sync::mpsc::SyncSender<Job>,
        batches: &std::sync::mpsc::Receiver<Batch>,
    ) -> Result<(), ConvertError> {
        self.marks.insert(job.index, job.chapters.clone());
        self.dispatched += 1;
        // A send fails only when every worker is gone, which they only are on their own failure.
        // There is nobody left to encode this job, so what is left to do is write the jobs that
        // did come back and state that one did not.
        if jobs.send(job).is_err() {
            return self.drain(batches);
        }

        while let Ok(batch) = batches.try_recv() {
            self.accept(batch)?;
        }

        Ok(())
    }

    /// Waits for and writes everything still out.
    ///
    /// What ends the wait is the pool ending: the workers let go of the channel as they run out of
    /// jobs, and the last of them to do so closes it. So a batch that never came back is a worker
    /// that failed rather than a wait that goes on forever, and the count of what went out against
    /// what was written is what says so.
    ///
    /// # Errors
    ///
    /// What a worker failed with, or what writing failed with.
    fn drain(&mut self, batches: &std::sync::mpsc::Receiver<Batch>) -> Result<(), ConvertError> {
        for batch in batches {
            self.accept(batch)?;
        }
        if self.next < self.dispatched {
            return Err(ConvertError::Io(std::io::Error::other(
                "an encoding worker failed",
            )));
        }

        Ok(())
    }

    /// Keeps a batch, and writes it and everything behind it that has arrived.
    ///
    /// # Errors
    ///
    /// What the worker that encoded a batch failed with, or what writing it failed with.
    fn accept(&mut self, (index, batch): Batch) -> Result<(), ConvertError> {
        self.pending.insert(index, batch);

        while let Some(batch) = self.pending.remove(&self.next) {
            let batch = batch?;
            for title in self.marks.remove(&self.next).unwrap_or_default() {
                self.sink.begin_chapter()?;
                place(
                    self.chapters,
                    ChapterOut {
                        page: self.sink.block(),
                        start: position(self.sink.frames()),
                        title,
                    },
                );
            }
            for packet in &batch {
                self.sink.push_packet(packet)?;
                (self.progress)(Progress::Encoded {
                    samples_done: self.sink.frames(),
                });
            }
            self.next += 1;
        }

        Ok(())
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
    use super::{
        Collector, ConvertError, Input, Job, PacketSink, Progress, WriterError, WriterIoError,
    };
    use crate::encode::FRAME;
    use std::io::{self, Cursor};
    use std::time::Duration;
    use taf::id::AudioId;

    /// A job of `index` carrying `chapters`. What its audio is stays empty: a collector writes the
    /// packets it is handed back, and never the samples they were encoded from.
    fn job(index: usize, chapters: Vec<Option<String>>) -> Job {
        Job {
            index,
            warmup: Vec::new(),
            pcm: Vec::new(),
            chapters,
        }
    }

    /// A packet the writer takes: the table-of-contents byte every Opus packet opens with, and a
    /// body of `body` bytes of `mark` behind it — which is what a test finds it in the file by,
    /// since padding the last packet of a file reframes it and carries its frame over as it is.
    fn packet(mark: u8, body: usize) -> Vec<u8> {
        let mut packet = vec![mark; body + 1];
        packet[0] = 0x0c;

        packet
    }

    #[test]
    fn batches_that_come_back_out_of_turn_are_written_in_the_order_the_chunks_were_cut() {
        let mut file = Cursor::new(Vec::new());
        let mut sink = PacketSink::new(AudioId::new(7), &mut file).unwrap();
        let mut chapters = Vec::new();
        let mut done = Vec::new();
        let mut progress = |event| {
            if let Progress::Encoded { samples_done } = event {
                done.push(samples_done);
            }
        };
        let (jobs, waiting) = std::sync::mpsc::sync_channel(2);
        let (worker, batches) = std::sync::mpsc::channel();

        {
            let mut collector = Collector::new(&mut sink, &mut chapters, &mut progress);
            collector
                .dispatch(job(0, Vec::new()), &jobs, &batches)
                .unwrap();
            collector
                .dispatch(job(1, vec![Some(String::from("Two"))]), &jobs, &batches)
                .unwrap();
            // The second chunk was encoded first, and its packets wait for the first chunk's.
            worker.send((1, Ok(vec![packet(22, 90)]))).unwrap();
            worker.send((0, Ok(vec![packet(11, 80)]))).unwrap();
            // Which is a pool that has run out of work and let go, and so a drain that ends.
            drop(worker);
            collector.drain(&batches).unwrap();
        }
        drop(waiting);
        sink.finish().unwrap();
        let written = file.into_inner();
        let at = |mark: u8, body: usize| {
            let run = vec![mark; body];
            written
                .windows(body)
                .position(|window| window == run)
                .expect("the packet went into the file")
        };

        // The file carries the chunks in the order they were cut, whatever order they came back in.
        assert!(at(11, 80) < at(22, 90), "the first chunk is first");
        assert_eq!(done, [u64::from(FRAME), 2 * u64::from(FRAME)]);
        // And the chapter of the second chunk begins in front of that chunk's own audio, which is
        // the block behind the page the first one's packet closed.
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title.as_deref(), Some("Two"));
        assert_eq!(chapters[0].page.get(), 1);
        assert_eq!(chapters[0].start, Duration::from_millis(60));
    }

    #[test]
    fn a_pool_that_is_gone_is_stated_rather_than_waited_for() {
        // Both places a worker that failed outright shows here: a job nothing is left to take, and
        // the batch of a job already out that is never coming back. Waiting for either would be
        // waiting forever, so what a conversion gets is the one failure a dead thread leaves.
        let mut file = Cursor::new(Vec::new());
        let mut sink = PacketSink::new(AudioId::new(7), &mut file).unwrap();
        let mut chapters = Vec::new();
        let mut progress = |_| {};
        let mut collector = Collector::new(&mut sink, &mut chapters, &mut progress);
        let (jobs, waiting) = std::sync::mpsc::sync_channel(1);
        let (worker, batches) = std::sync::mpsc::channel();
        drop(waiting);
        drop(worker);

        let refusal = collector
            .dispatch(job(0, Vec::new()), &jobs, &batches)
            .expect_err("there is nobody left to encode the job");

        assert_eq!(refusal.to_string(), "output i/o failed");
        assert!(
            matches!(&refusal, ConvertError::Io(failure) if failure.to_string() == "an encoding worker failed"),
            "{refusal:?}"
        );
        assert!(chapters.is_empty(), "nothing of that job was written");
    }

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
