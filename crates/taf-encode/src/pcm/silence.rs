//! The last stage that changes which samples a TAF holds: [`SilenceProcessor`] takes the silence a
//! recording begins a chapter with off the front of it, puts a measured pause there instead, and
//! moves every chapter mark to where its audio ended up.
//!
//! # Frames, not samples
//!
//! Every count and every offset here — what the options state, the chapter starts that go in, the
//! adjusted ones that come out — is in *frames* at 48 kHz: one sample per channel, two `i16` in
//! the interleaved stream. That is the unit [`Pcm48::scale_samples`] answers in and the one a
//! chapter mark has been counted in since it was read out of a container, so nothing on the way
//! through here is scaled, halved or doubled.
//!
//! # What silence is
//!
//! A frame is silence while both of its samples stay below [`SILENCE_THRESHOLD`] in magnitude, and
//! sound as soon as the louder of the two reaches it. The threshold is one documented constant
//! rather than a knob: the level below which a recording is idling rather than speaking is a
//! property of how audiobooks are made, and a knob nobody can set correctly is only a way to get a
//! chapter's first word cut off.
//!
//! The rule is per frame rather than per sample, because a frame is what is played: a click on the
//! left channel is audible, so a frame is not silence for having a zero on the other side of it.
//!
//! # What happens in which order
//!
//! 1. **The leading skip** drops frames off the absolute front of the stream — a publisher's
//!    jingle, an intro nobody wants to hear twice — before anything else has looked at them.
//! 2. **The trim** drops the silence a chapter begins with: from the chapter's start up to the
//!    first frame of it that is sound.
//! 3. **The pause** puts exactly as much silence in front of that chapter's audio as was asked
//!    for.
//!
//! Which is what makes a worked example exact — a skip of 4.4 s, a trim and a pause of 1.0 s
//! leave one second of silence in front of the first sound there is, to the frame. The skip takes
//! off what nobody wants, the trim takes off however long the recording happened to idle for, and
//! the pause is then the only silence left to hear.
//!
//! # Where a chapter mark lands
//!
//! On the first frame of its pause, and not on its first audible one. The pause belongs to the
//! chapter it was inserted at, for two reasons: the first chapter of a TAF begins at offset 0, and
//! there is no room in front of it for silence belonging to nothing; and a listener who skips to a
//! chapter should get that chapter, pause and all, rather than have its pause played as the tail
//! of the chapter before it.
//!
//! # Chapters that come out in the same place
//!
//! A chapter that is nothing but silence trims to no length at all, one the leading skip ran past
//! never had a frame of its own, and one that begins behind the end of the stream never begins.
//! None of them is dropped: every chapter that went in comes out, in order, at the place where its
//! audio would have begun — which is the place the chapter behind it begins. So the adjusted
//! offsets are non-decreasing rather than increasing, and a caller turning them into marks has to
//! be ready for two of them in one place.
//!
//! # Reading the offsets while the stream runs
//!
//! [`SilenceProcessor::adjusted_chapters`] answers only once the stream has ended, because the
//! place of a chapter that never began is not settled until the last frame is out. A caller that
//! writes chapter marks as it goes cannot wait for that, so
//! [`chapters_emitted`](SilenceProcessor::chapters_emitted) hands out what is settled already: a
//! chapter's place is final the moment the chapter begins, which is in front of every frame at or
//! behind it. What that gives a caller is this — when a block comes out of
//! [`next_block`](SilenceProcessor::next_block), every chapter mark at or in front of that block's
//! first frame is in the list, and none behind it is. A block never spans a chapter start either:
//! where a chapter begins, a block ends.
//!
//! # What is not done here
//!
//! Nothing fades and nothing is levelled. An inserted pause is exact zeros, and every frame that
//! is kept is the frame that went in, sample for sample. A trim cuts on a frame boundary with
//! nothing eased in behind it, which is inaudible by construction: the frame in front of the cut
//! was below −55 dBFS, which is what made it silence.

use super::{Pcm48, PcmError, CHANNELS};

/// The quietest sample that counts as sound: −55 dBFS of `i16` full scale, which is
/// 32 768 · 10<sup>−55/20</sup> ≈ 58.3, at the sample below that.
///
/// The comparison is inclusive on this side of it: a frame is sound as soon as the louder of its
/// two samples *reaches* this in magnitude, and silence while both of them stay below it — ±58 is
/// sound and ±57 is silence.
///
/// It is a documented constant and not a knob.
pub const SILENCE_THRESHOLD: i16 = 58;

/// How many frames of an inserted pause are handed out at once: a tenth of a second, so that a
/// pause of any length costs one small block at a time instead of one allocation the size of the
/// whole pause.
const PAUSE_FRAMES: usize = 4_800;

/// What the silence operations are asked to do.
///
/// Every count is in frames at 48 kHz, and everything is off by default: [`SilenceOpts::default`]
/// hands a stream through exactly as it came.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SilenceOpts {
    /// How many frames to drop from the absolute start of the stream, in front of everything else
    /// — including the first chapter's trim, which then begins where the skip ended.
    pub skip_leading: u64,
    /// Whether to drop the silence the first chapter begins with.
    pub trim_leading: bool,
    /// Whether to drop the silence every chapter begins with, the first one included — so this
    /// covers [`trim_leading`](Self::trim_leading) on its own.
    pub trim_each_chapter: bool,
    /// How many frames of silence to put in front of the first chapter's audio, after its trim.
    /// Added to [`add_pause_each_chapter`](Self::add_pause_each_chapter) where both are asked for:
    /// each option states what it puts in, and neither takes the other's place.
    pub add_pause_leading: u64,
    /// How many frames of silence to put in front of every chapter's audio, after that chapter's
    /// own trim — a chapter that trimmed to nothing at all included.
    pub add_pause_each_chapter: u64,
}

impl SilenceOpts {
    /// Whether the chapter beginning here has the silence it begins with dropped.
    fn trims(self, first: bool) -> bool {
        self.trim_each_chapter || (first && self.trim_leading)
    }

    /// How many frames of silence go in front of the audio of the chapter beginning here.
    fn pause(self, first: bool) -> u64 {
        let leading = if first { self.add_pause_leading } else { 0 };

        self.add_pause_each_chapter.saturating_add(leading)
    }
}

/// What the machine does with the input frames in front of it.
enum Phase {
    /// Dropping the silence a chapter begins with, up to the first frame of it that is sound.
    Trimming,
    /// Handing frames on as they came, up to the start of the next chapter.
    Passing,
}

/// A 48 kHz stereo stream with the silence operations applied to it, and the chapter marks that go
/// with it.
///
/// Built over a [`Pcm48`] and the chapter starts of the stream it hands out, pulled with
/// [`next_block`](Self::next_block) until it states the end of the stream — at which point
/// [`adjusted_chapters`](Self::adjusted_chapters) states where every one of those chapters landed.
pub struct SilenceProcessor {
    /// Where the samples come from.
    pcm: Pcm48,
    /// Where the chapters begin in the stream that goes in, in frames.
    chapters: Vec<u64>,
    /// What to do at the start of the stream and at the start of every chapter.
    opts: SilenceOpts,
    /// The block being consumed, and how many of its samples are behind the machine already.
    block: Vec<i16>,
    at: usize,
    /// How many frames of the input are behind the machine, the dropped ones included.
    in_pos: u64,
    /// How many frames have been handed out.
    out_pos: u64,
    /// Where the chapters that have begun landed. One entry goes in per chapter begun, so its
    /// length is also the index of the chapter to begin next.
    adjusted: Vec<u64>,
    /// How many frames of an inserted pause are still to be handed out.
    owed: u64,
    /// What the machine does with the input frames in front of it.
    phase: Phase,
    /// Whether the stream has ended, which is where the last chapter's place is settled.
    ended: bool,
}

impl SilenceProcessor {
    /// A processor over `pcm`, whose chapters begin at `chapters`.
    ///
    /// The offsets are frames at 48 kHz, counted from the start of `pcm`'s stream — what
    /// [`Pcm48::scale_samples`] answers in — and are expected in order. An offset out of order is
    /// no error and nothing panics on one: a chapter begins at the first frame at or behind its
    /// start, so one that lies behind a chapter already begun begins at once.
    #[must_use]
    pub fn new(pcm: Pcm48, chapters: Vec<u64>, opts: SilenceOpts) -> Self {
        Self {
            pcm,
            chapters,
            opts,
            block: Vec::new(),
            at: 0,
            in_pos: 0,
            out_pos: 0,
            adjusted: Vec::new(),
            owed: 0,
            phase: Phase::Passing,
            ended: false,
        }
    }

    /// The next block of the processed stream, or `Ok(None)` at the end of it.
    ///
    /// A block is whole frames and never empty, and it never spans a chapter start: where a
    /// chapter begins, a block ends. How many frames one holds is not a promise — it is neither
    /// the block length of the stream underneath nor a fixed one of this stage's.
    ///
    /// # Errors
    ///
    /// [`PcmError`] as the stream underneath stated it. The silence operations themselves cannot
    /// fail: dropping and inserting frames is arithmetic on counts the caller stated.
    pub fn next_block(&mut self) -> Result<Option<Vec<i16>>, PcmError> {
        loop {
            if let Some(pause) = self.pause() {
                return Ok(Some(pause));
            }
            if self.frames_left() == 0 {
                let Some(block) = self.pcm.next_block()? else {
                    self.end();

                    return Ok(None);
                };
                self.block = block;
                self.at = 0;

                continue;
            }
            if self.chapter_due() {
                self.begin_chapter();

                continue;
            }
            if let Some(block) = self.take() {
                return Ok(Some(block));
            }
        }
    }

    /// Where every chapter that has begun landed in the stream handed out so far.
    ///
    /// A growing prefix of what [`adjusted_chapters`](Self::adjusted_chapters) states at the end,
    /// and the answer for a caller that has to place a chapter mark while the stream is still
    /// running: a chapter's place is settled the moment the chapter begins, which is in front of
    /// every frame of it. So when a block comes out of [`next_block`](Self::next_block), this
    /// holds every mark at or in front of that block's first frame and none behind it.
    #[must_use]
    pub fn chapters_emitted(&self) -> &[u64] {
        &self.adjusted
    }

    /// Where every chapter landed in the stream this handed out, once there is no more of it.
    ///
    /// `None` while the stream is still running, because a chapter whose audio was all trimmed
    /// away — or that lies behind the end of the stream — lands where the stream ends, which is
    /// not known before then. There is one offset per chapter that went in, in the same order, and
    /// two chapters can land in the same place.
    #[must_use]
    pub fn adjusted_chapters(&self) -> Option<&[u64]> {
        self.ended.then_some(self.adjusted.as_slice())
    }

    /// The next block of a pause being inserted, while one is still owed.
    fn pause(&mut self) -> Option<Vec<i16>> {
        if self.owed == 0 {
            return None;
        }

        let frames = narrow(self.owed).min(PAUSE_FRAMES);
        self.owed -= wide(frames);
        self.out_pos += wide(frames);

        Some(vec![0; frames * usize::from(CHANNELS)])
    }

    /// Whether the chapter in front of the machine begins at the frame in front of it.
    ///
    /// Nothing begins inside the leading skip: a chapter the skip ran past begins where the skip
    /// ended, together with every other chapter it ran past.
    fn chapter_due(&self) -> bool {
        self.in_pos >= self.opts.skip_leading
            && self
                .chapters
                .get(self.adjusted.len())
                .is_some_and(|start| self.in_pos >= *start)
    }

    /// A chapter begins here: its mark lands where the stream handed out so far ends, the pause it
    /// was asked for is owed from this frame on, and the silence it begins with goes if it was
    /// asked for.
    ///
    /// The pause is owed whatever the trim then finds, a chapter that trims to nothing included:
    /// what was asked for is silence in front of the chapter, not silence in front of its audio
    /// only where there is some.
    fn begin_chapter(&mut self) {
        let first = self.adjusted.is_empty();
        self.adjusted.push(self.out_pos);
        self.owed = self.owed.saturating_add(self.opts.pause(first));
        self.phase = if self.opts.trims(first) {
            Phase::Trimming
        } else {
            Phase::Passing
        };
    }

    /// What becomes of the frames in front of the machine: dropped by the skip, dropped by a trim,
    /// or handed on as they came. Never more of them than the block it holds, and never past the
    /// start of the next chapter.
    fn take(&mut self) -> Option<Vec<i16>> {
        let left = self.frames_left();
        if self.in_pos < self.opts.skip_leading {
            self.advance(left.min(narrow(self.opts.skip_leading - self.in_pos)));

            return None;
        }

        let frames = left.min(self.to_next_chapter());
        match self.phase {
            Phase::Trimming => {
                let silent = self.silent_frames(frames);
                self.advance(silent);
                if silent == frames {
                    // Every frame there was to look at was silence, so the trim goes on into
                    // whatever comes behind them.
                    return None;
                }
                // And behind them is a frame that is not, which is where the trim ends: from there
                // to the end of the chapter everything is handed on, silence in the middle of it
                // included.
                self.phase = Phase::Passing;

                Some(self.emit(frames - silent))
            }
            Phase::Passing => Some(self.emit(frames)),
        }
    }

    /// How many of the `frames` frames in front of the machine are silence, up to the first one
    /// that is not.
    fn silent_frames(&self, frames: usize) -> usize {
        self.rest()
            .chunks_exact(usize::from(CHANNELS))
            .take(frames)
            .take_while(|frame| is_silent(frame))
            .count()
    }

    /// How many frames are left in front of the start of the next chapter — every frame there is,
    /// where no chapter is left to begin.
    fn to_next_chapter(&self) -> usize {
        self.chapters
            .get(self.adjusted.len())
            .map_or(usize::MAX, |start| {
                narrow(start.saturating_sub(self.in_pos))
            })
    }

    /// The samples of the block the machine holds that are still in front of it.
    fn rest(&self) -> &[i16] {
        self.block.get(self.at..).unwrap_or_default()
    }

    /// How many whole frames are still in front of the machine.
    fn frames_left(&self) -> usize {
        self.rest().len() / usize::from(CHANNELS)
    }

    /// Puts `frames` frames behind the machine without handing them out: they are dropped.
    fn advance(&mut self, frames: usize) {
        self.at += frames * usize::from(CHANNELS);
        self.in_pos += wide(frames);
    }

    /// The next `frames` frames as they came, on their way out.
    fn emit(&mut self, frames: usize) -> Vec<i16> {
        let block = self
            .rest()
            .get(..frames * usize::from(CHANNELS))
            .unwrap_or_default()
            .to_vec();
        self.advance(frames);
        self.out_pos += wide(frames);

        block
    }

    /// The end of the stream: every chapter that never began lands where the stream ended, and the
    /// offsets are all there from here on.
    fn end(&mut self) {
        self.adjusted.resize(self.chapters.len(), self.out_pos);
        self.ended = true;
    }
}

/// Whether a frame is silence: both of its samples below [`SILENCE_THRESHOLD`] in magnitude.
///
/// The frame's peak is the louder of its two sides, so a frame with a click on one channel and a
/// zero on the other is sound. Magnitudes are taken in `i32`, where the loudest sample there is
/// has one.
fn is_silent(frame: &[i16]) -> bool {
    let peak = frame
        .iter()
        .map(|sample| i32::from(*sample).abs())
        .max()
        .unwrap_or(0);

    peak < i32::from(SILENCE_THRESHOLD)
}

/// A count of frames as a position in the stream is counted in.
fn wide(frames: usize) -> u64 {
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// A position in the stream as a count of frames in a block is counted in. Nothing that far into a
/// stream fits in a block either, so pinning it at what a block could hold changes no answer.
fn narrow(frames: u64) -> usize {
    usize::try_from(frames).unwrap_or(usize::MAX)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{is_silent, SilenceOpts, SILENCE_THRESHOLD};

    #[test]
    fn a_frame_at_the_threshold_is_sound_and_one_below_it_is_silence() {
        assert!(is_silent(&[SILENCE_THRESHOLD - 1, SILENCE_THRESHOLD - 1]));
        assert!(is_silent(&[-(SILENCE_THRESHOLD - 1), 0]));
        assert!(is_silent(&[0, 0]));
        assert!(!is_silent(&[SILENCE_THRESHOLD, 0]));
        assert!(!is_silent(&[0, -SILENCE_THRESHOLD]));
        // And the two loudest samples there are, one of which has no positive counterpart.
        assert!(!is_silent(&[i16::MAX, i16::MAX]));
        assert!(!is_silent(&[i16::MIN, 0]));
    }

    #[test]
    fn the_louder_of_the_two_channels_decides_whether_a_frame_is_silence() {
        assert!(!is_silent(&[100, 0]));
        assert!(!is_silent(&[0, 100]));
        assert!(!is_silent(&[-100, 1]));
        assert!(is_silent(&[SILENCE_THRESHOLD - 1, 0]));
    }

    #[test]
    fn every_chapter_is_trimmed_when_asked_and_the_first_one_on_its_own_option_too() {
        let leading = SilenceOpts {
            trim_leading: true,
            ..SilenceOpts::default()
        };
        let each = SilenceOpts {
            trim_each_chapter: true,
            ..SilenceOpts::default()
        };

        assert!(leading.trims(true));
        assert!(!leading.trims(false));
        assert!(each.trims(true));
        assert!(each.trims(false));
        assert!(!SilenceOpts::default().trims(true));
        assert!(!SilenceOpts::default().trims(false));
    }

    #[test]
    fn the_first_chapter_is_given_both_pauses_and_every_other_one_only_its_own() {
        let opts = SilenceOpts {
            add_pause_leading: 1_200,
            add_pause_each_chapter: 2_400,
            ..SilenceOpts::default()
        };

        assert_eq!(opts.pause(true), 3_600);
        assert_eq!(opts.pause(false), 2_400);
        assert_eq!(SilenceOpts::default().pause(true), 0);
        // A pause nothing could hold is pinned at what fits rather than wrapping to none at all.
        let boundless = SilenceOpts {
            add_pause_leading: u64::MAX,
            add_pause_each_chapter: u64::MAX,
            ..SilenceOpts::default()
        };
        assert_eq!(boundless.pause(true), u64::MAX);
    }
}
