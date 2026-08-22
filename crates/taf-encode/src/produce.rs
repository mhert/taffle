//! The reading half of a conversion: [`produce`] opens the inputs, brings what they hold to the
//! 48 kHz stereo a TAF carries, and hands the blocks over a channel instead of into an encoder.
//!
//! # Why the reading runs on a thread of its own
//!
//! Decoding a block and encoding the block in front of it are two jobs that need nothing from each
//! other, and a conversion that does them one after the other spends every decode with the encoder
//! idle and every encode with the decoder idle. So the reading runs here, ahead of the writing, as
//! far ahead as the channel between them is deep — and a conversion takes about as long as its
//! slower half rather than as long as both of them added up.
//!
//! # What that changes about the file, which is nothing
//!
//! [`Feed`] is the sequence of calls the encoder used to be handed directly, in the order it used
//! to be handed them: a chapter is begun in front of the block behind it, and blocks travel in the
//! order they play. A thread decides *when* the work happens and never what it comes to, so the
//! file is byte for byte the file it was.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::SyncSender;
use std::vec::IntoIter;

use crate::chapters::ChapterMode;
use crate::convert::{Conversion, ConvertError, Input, CHANNELS, RATE};
use crate::decode::{open_source, AudioSource, Cover, DecodeError, SourceMetadata, SourceSpec};
use crate::pcm::{Pcm48, PcmError, SilenceOpts, SilenceProcessor};

/// What the reading hands the encoding, in the order the encoding takes it.
pub(crate) enum Feed {
    /// The conversion has reached input `0` and is reading it, counted over all inputs.
    Reached(usize),
    /// A chapter begins in front of the next block, under this title.
    Chapter(Option<String>),
    /// The next interleaved 48 kHz stereo samples.
    Block(Vec<i16>),
}

/// What the reading knew only once it was over, and the conversion needs when the audio is in.
pub(crate) struct Produced {
    /// The frames of 48 kHz stereo the inputs decoded to — the unit an explicit offset is
    /// checked against.
    pub frames: u64,
    /// The cover art of the first input that carried any.
    pub cover: Option<Cover>,
    /// The title of the mark at offset 0, if an input authored one — what the opening chapter
    /// of a file with no audio is called.
    pub opening_title: Option<String>,
}

/// Reads `inputs` the way `opts` asks for and hands what comes of them down `feed`.
///
/// This is the conversion's outer loop with the encoder calls replaced by sends, so the plan is
/// decided and the silence operations are applied exactly where they were: what crosses the channel
/// is what the encoder was handed before.
///
/// A send that fails means the receiver was dropped — the conversion on the other end failed and
/// holds its own error, so there is nobody left to read anything further and the reading stops with
/// what it has.
///
/// # Errors
///
/// [`ConvertError::Input`] if an input could not be read, decoded, or brought to 48 kHz stereo,
/// naming the input it happened in.
pub(crate) fn produce(
    inputs: Vec<Input>,
    opts: &Conversion,
    feed: &SyncSender<Feed>,
) -> Result<Produced, ConvertError> {
    let names: Vec<String> = inputs.iter().map(|input| input.name.clone()).collect();
    let reading = Rc::new(Reading::default());
    // What a failure of the stream says: the input it happened in, and the failure the
    // concatenation kept where the trait it hands blocks over had no room for it.
    let failed = |source: PcmError| ConvertError::Input {
        name: names.get(reading.at.get()).cloned().unwrap_or_default(),
        source: reading.kept().unwrap_or(source),
    };

    let mut opening_title = None;
    let mut reported = 0;
    let streams = streamed(inputs, &opts.chapter_mode);
    let per_input = streams.len() > 1;

    'streams: for (base, inputs) in streams {
        let mut concat = Concat::new(inputs, base, Rc::clone(&reading));
        let marks = concat.prime().map_err(&failed)?;
        if !reach(&reading, &mut reported, feed) {
            break 'streams;
        }

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
            if !reach(&reading, &mut reported, feed) {
                break 'streams;
            }

            // Every chapter whose place is settled begins in front of this block: the frame in
            // hand is filled out and the chapter starts the block behind it.
            let settled = stream.chapters_emitted().len();
            for at in begun..settled {
                let title = plan.get(at).and_then(|chapter| chapter.title.clone());
                if feed.send(Feed::Chapter(title)).is_err() {
                    break 'streams;
                }
            }
            begun = settled;

            if feed.send(Feed::Block(block)).is_err() {
                break 'streams;
            }
        }

        // An input that handed out no blocks at all was still reached.
        if !reach(&reading, &mut reported, feed) {
            break 'streams;
        }
    }

    Ok(Produced {
        frames: reading.frames.get(),
        cover: reading.cover.take(),
        opening_title,
    })
}

/// Reports every input reached since the last report. `false` means the consumer is gone and
/// producing anything further is work nobody reads.
///
/// An input is reached once, in the order they play: the concatenation states which one it is
/// reading, and everything from the last one reported to that one is announced here.
fn reach(reading: &Reading, reported: &mut usize, feed: &SyncSender<Feed>) -> bool {
    let at = reading.at.get();

    for input_index in *reported..=at {
        if feed.send(Feed::Reached(input_index)).is_err() {
            return false;
        }
    }
    *reported = at.saturating_add(1);

    true
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
/// another, under the name the first mark there carried.
///
/// Marks come out of a container in the order the container states them, which is not necessarily
/// the order they play in — so they are sorted here, and the sort is stable, which is what makes
/// the first mark stated at a place the one that names it.
///
/// The half of the rule that needs a length is the stream's: a mark at or behind the end of the
/// audio never begins a chapter, because the stream ends in front of the block it would have begun
/// — and nothing knows where that end is until the audio has run out.
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{
        produce, AudioSource, ChapterMode, Concat, Conversion, Feed, Input, Produced, Reading,
        SourceMetadata,
    };
    use std::io::Cursor;
    use std::rc::Rc;
    use std::sync::mpsc::sync_channel;

    /// The frames of 48 kHz stereo a sounding input here holds: a second of them, which is more
    /// than one block, so a reading that stopped inside one is told apart from one that ran out.
    const FRAMES: usize = 48_000;

    /// An input of `frames` frames of 48 kHz stereo, as a WAV a decoder opens.
    ///
    /// The samples are silence: what is in them decides nothing here, and a reading that hands
    /// blocks over hands over the ones it read whatever they hold.
    fn sound(frames: usize) -> Input {
        let data = u32::try_from(frames * 4).unwrap();
        let mut wav = Vec::with_capacity(44 + data as usize);

        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data).to_le_bytes());
        wav.extend_from_slice(b"WAVE");

        // The format chunk: 16 bytes of PCM description, two channels of 16 bits at 48 kHz.
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&48_000_u32.to_le_bytes());
        wav.extend_from_slice(&192_000_u32.to_le_bytes());
        wav.extend_from_slice(&4_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());

        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data.to_le_bytes());
        wav.resize(44 + data as usize, 0);

        Input {
            reader: Box::new(Cursor::new(wav)),
            name: format!("{frames}.wav"),
        }
    }

    /// Reads `inputs` over a channel that hands one feed across at a time, stops taking after
    /// `take` of them, and hands over what was taken and what the reading came to.
    ///
    /// A channel of no depth is what makes this exact: nothing is handed over until it is taken, so
    /// the reading stands at the feed behind the last one taken when the channel is dropped — which
    /// is where a conversion whose encoder refused a frame leaves it.
    fn taken(inputs: Vec<Input>, opts: &Conversion, take: usize) -> (Vec<Feed>, Produced) {
        std::thread::scope(|scope| {
            let (tx, rx) = sync_channel(0);
            let reading = scope.spawn(move || produce(inputs, opts, &tx));
            let feeds: Vec<Feed> = rx.iter().take(take).collect();
            drop(rx);

            (feeds, reading.join().unwrap().unwrap())
        })
    }

    /// What a feed is, in a word a test compares.
    fn named(feed: &Feed) -> String {
        match feed {
            Feed::Reached(at) => format!("reached {at}"),
            Feed::Chapter(title) => format!("chapter {title:?}"),
            // What is in a block is the sample stage's business; that one came across where it
            // did is this module's.
            Feed::Block(_) => String::from("block"),
        }
    }

    /// The inputs as one stream, which is what an explicit plan makes of them — so that the input
    /// behind the one that ran out is reached inside the stream rather than in front of a new one.
    fn one_stream() -> Conversion {
        Conversion {
            chapter_mode: ChapterMode::Explicit(Vec::new()),
            ..Conversion::default()
        }
    }

    #[test]
    fn a_chapter_is_handed_over_in_front_of_the_block_it_begins() {
        // Which is the whole of what the encoding on the other end is promised: the input it is
        // reading, then the chapter the audio opens with, then the audio — the order the encoder
        // used to be called in, and so the order the file comes out in.
        let (feeds, _) = taken(vec![sound(FRAMES)], &Conversion::default(), 3);

        assert_eq!(
            feeds.iter().map(named).collect::<Vec<_>>(),
            ["reached 0", "chapter None", "block"]
        );
        assert!(
            matches!(feeds.get(2), Some(Feed::Block(block)) if !block.is_empty()),
            "the block carries the samples that were read"
        );
    }

    #[test]
    fn a_reading_nobody_takes_from_any_more_stops_where_it_stands() {
        // Every place the reading hands something over, with the conversion on the other end gone
        // by the time it does: in front of the audio, in front of a chapter, at the input behind
        // the one that ran out, and at the last input of all.
        let opening = taken(vec![sound(FRAMES)], &Conversion::default(), 0);
        let chapter = taken(vec![sound(FRAMES)], &Conversion::default(), 1);
        let behind = taken(vec![sound(0), sound(FRAMES)], &one_stream(), 1);
        let last = taken(vec![sound(0), sound(0)], &one_stream(), 1);

        // Nothing was read for a conversion that was over before the first input was announced.
        assert!(opening.0.is_empty());
        assert_eq!(opening.1.frames, 0);

        // And where it stopped further in, it stopped with what it had rather than reading on:
        // the block in hand was decoded and the rest of the audio never was.
        for (feeds, produced) in [chapter, behind] {
            assert_eq!(feeds.iter().map(named).collect::<Vec<_>>(), ["reached 0"]);
            assert!(
                (1..FRAMES as u64).contains(&produced.frames),
                "the reading stopped inside the input: {} frames",
                produced.frames
            );
        }

        // The last input of all is announced when the audio has run out, and there was nobody
        // left to tell.
        assert_eq!(last.0.iter().map(named).collect::<Vec<_>>(), ["reached 0"]);
        assert_eq!(last.1.frames, 0);
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
