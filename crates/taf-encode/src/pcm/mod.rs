//! What every input becomes before anything encodes it: [`Pcm48`], the 48 kHz stereo stream a TAF
//! carries, pulled a block at a time out of whatever an [`AudioSource`] decoded to.
//!
//! # One rate and two channels, and why they are these
//!
//! A TAF holds Ogg-Opus at 48 kHz in two channels, and Opus is *defined* at 48 kHz — so everything
//! behind this stage, the silence operations and the encoder both, is written for that one shape.
//! A source keeps the rate and the channel count it was authored at, which is what [`SourceSpec`](crate::SourceSpec)
//! states; this is where every one of them converges on the shape a TAF is in, and it is the last
//! place where anything about the samples is not yet what the file will hold.
//!
//! # A source that is already there goes past the resampler, not through it
//!
//! An input that decodes to 48 kHz stereo — every Ogg-Opus file, since Opus is decoded at the rate
//! it is defined at — is handed on block for block and sample for sample. Resampling it by a ratio
//! of one would still put it through a filter, and a filter that is not needed is one that only
//! has something to take away.
//!
//! # A mono source is one signal until the last moment
//!
//! One channel becomes two by being handed out twice, and that happens *behind* the resampler
//! rather than in front of it: the same wave resampled twice is the same arithmetic done twice,
//! and two channels that were computed apart can only ever be the same by coincidence. A mono
//! source is resampled once, as the one signal it is, and the two sides a TAF carries are that
//! signal — sample for sample, by construction.
//!
//! # A stream is exactly as long as `scale_samples` says it is
//!
//! [`Pcm48::scale_samples`] is what a position at the source's rate becomes at 48 kHz, and it is
//! not only for chapter marks: the stream a stage hands out holds exactly `scale_samples(frames)`
//! frames, where `frames` is what the source handed out. A resampler goes on giving back frames
//! after the last of the recording has gone into it — a filter's tail over samples that are not
//! audio — and that is where the stream is cut. So a chapter mark scaled by that function lands
//! where the audio around it went, and a book does not drift a frame away from what it was.
//!
//! # What is not done here
//!
//! Nothing dithers the samples on the way back to `i16`, and nothing touches their level. The
//! quantization error a resampled sample carries sits around −96 dBFS, which is below the noise
//! floor of anything an audiobook was recorded at and far below what the lossy codec behind this
//! keeps of it either way.

mod resample;

use std::num::NonZeroU32;

use resample::Resampler48;

use crate::decode::{AudioSource, DecodeError};

/// The rate every block this stage hands out is in: what a TAF carries, and what Opus is defined
/// at.
const RATE: u32 = 48_000;

/// How many channels every block this stage hands out interleaves.
const CHANNELS: u16 = 2;

/// The slowest source this stage resamples from — a tenth of [`RATE`], which is under the 8 kHz
/// the narrowest speech recording is authored at.
const SLOWEST: u32 = RATE / 10;

/// The fastest source this stage resamples from — ten times [`RATE`], which is over the 192 kHz
/// the widest studio recording is authored at.
///
/// The band the two of them make is what keeps one turn of the resampler bounded: it takes a
/// fixed number of source frames, so a rate far below this would make one turn's output enormous
/// and a rate far above it would leave one turn shorter than the filter can reach across.
const FASTEST: u32 = RATE * 10;

/// A source's audio in the one shape everything behind this stage works in: 48 kHz, two channels,
/// interleaved `i16`.
///
/// Built over any [`AudioSource`], pulled with [`next_block`](Self::next_block) until it states
/// the end of the stream.
pub struct Pcm48 {
    /// Where the samples come from.
    source: Box<dyn AudioSource>,
    /// The rate the source states, which is what a position of its is counted in.
    rate: NonZeroU32,
    /// What the source's blocks go through on the way out.
    stage: Stage,
    /// Whether the source has stated the end of its stream, which is where this one ends too.
    ended: bool,
}

impl Pcm48 {
    /// A stage over `source`, if its audio is a shape this can bring to 48 kHz stereo.
    ///
    /// # Errors
    ///
    /// [`PcmError::UnsupportedChannels`] when the source states no channels at all or more than
    /// the two a TAF carries — a surround mix is not something to fold down without being told
    /// how. [`PcmError::UnsupportedRate`] when it states a rate outside the band between a tenth
    /// and ten times the 48 kHz it would be resampled to, which no recording is authored at.
    pub fn new(source: Box<dyn AudioSource>) -> Result<Self, PcmError> {
        let spec = source.spec();

        if spec.channels == 0 || spec.channels > CHANNELS {
            return Err(PcmError::UnsupportedChannels(spec.channels));
        }
        let rate = NonZeroU32::new(spec.sample_rate)
            .filter(|rate| (SLOWEST..=FASTEST).contains(&rate.get()))
            .ok_or(PcmError::UnsupportedRate(spec.sample_rate))?;

        let stage = if rate.get() == RATE {
            Stage::Through(Through::over(spec.channels))
        } else {
            Stage::Resampling(Box::new(Resampler48::new(rate, spec.channels)?))
        };

        Ok(Self {
            source,
            rate,
            stage,
            ended: false,
        })
    }

    /// The next block of interleaved 48 kHz stereo, or `Ok(None)` at the end of the stream.
    ///
    /// A block is whole frames and never empty; how many frames it holds is not a promise, and is
    /// neither the source's block length nor a fixed one of this stage's.
    ///
    /// # Errors
    ///
    /// [`PcmError::Decode`] as the source stated it — the end of the stream is not one of those.
    /// [`PcmError::Resample`] when the resampler refuses a turn, which is a chunk of the shape it
    /// was built for and so nothing a source can bring about.
    pub fn next_block(&mut self) -> Result<Option<Vec<i16>>, PcmError> {
        while !self.ended {
            let Some(block) = self.source.next_block()? else {
                self.ended = true;

                return Ok(self.stage.finish()?.filter(|block| !block.is_empty()));
            };

            let out = self.stage.push(block)?;
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }

        Ok(None)
    }

    /// Where a position of the source's lands in the stream this hands out.
    ///
    /// Both are counted in frames from the start of the stream — one sample per channel each, the
    /// unit [`SourceChapter::start_sample`](crate::SourceChapter::start_sample) is in — and the
    /// answer is rounded to the nearest frame, halves away from zero. A position further out than
    /// a stream of a hundred million years could reach is pinned at what fits.
    #[must_use]
    pub fn scale_samples(&self, source_samples: u64) -> u64 {
        at_48k(source_samples, self.rate)
    }
}

/// What a source's blocks go through on the way out.
enum Stage {
    /// A source that already states 48 kHz: its samples are handed on as they are, with the second
    /// channel of a mono one the only thing added to them.
    Through(Through),
    /// A source at any other rate, resampled a chunk at a time. Boxed because a resampler is the
    /// buffers it works in and a stage that needs none of them should not carry their room.
    Resampling(Box<Resampler48>),
}

impl Stage {
    /// What one block of the source amounts to, which is nothing at all while a resampler is still
    /// short of a chunk.
    ///
    /// # Errors
    ///
    /// [`PcmError::Resample`] as the resampler stated it.
    fn push(&mut self, block: Vec<i16>) -> Result<Vec<i16>, PcmError> {
        match self {
            Self::Through(through) => Ok(through.push(block)),
            Self::Resampling(resampler) => resampler.push(&block),
        }
    }

    /// What is left once the source has stated the end of its stream: nothing for a source that
    /// was handed through, and whatever the filter still holds for one that was resampled.
    ///
    /// # Errors
    ///
    /// [`PcmError::Resample`] as the resampler stated it.
    fn finish(&mut self) -> Result<Option<Vec<i16>>, PcmError> {
        match self {
            Self::Through(_) => Ok(None),
            Self::Resampling(resampler) => resampler.finish().map(Some),
        }
    }
}

/// A source that is already at 48 kHz, on its way out unchanged.
struct Through {
    /// How many channels the source interleaves its blocks over: one or two.
    channels: u16,
    /// What a block that ended in the middle of a frame left of it, which belongs in front of the
    /// block behind it.
    carry: Vec<i16>,
}

impl Through {
    /// A stage over a source of `channels` channels.
    fn over(channels: u16) -> Self {
        Self {
            channels,
            carry: Vec::new(),
        }
    }

    /// One block of the source as two channels at 48 kHz.
    fn push(&mut self, block: Vec<i16>) -> Vec<i16> {
        if self.channels == CHANNELS {
            return self.whole_frames(block);
        }

        // One channel, so every sample is a frame of its own — and the frame is that sample on
        // both sides.
        block.iter().flat_map(|sample| [*sample, *sample]).collect()
    }

    /// The block as it came, with whatever the block in front of it ended mid-frame in front of it
    /// and whatever this one ends mid-frame kept back for the block behind it.
    ///
    /// A source hands out whole frames — that is what interleaving means — but the trait states
    /// nothing about where its blocks end, and a stage that hands out half a frame has moved every
    /// sample behind it into the wrong channel.
    fn whole_frames(&mut self, mut block: Vec<i16>) -> Vec<i16> {
        let channels = usize::from(CHANNELS);
        if self.carry.is_empty() && block.len().is_multiple_of(channels) {
            return block;
        }

        let mut samples = std::mem::take(&mut self.carry);
        samples.append(&mut block);
        let whole = samples.len() - samples.len() % channels;
        self.carry = samples.split_off(whole);

        samples
    }
}

/// Where `frames` frames at `rate` land in a stream of [`RATE`] frames a second.
///
/// Rounded to the nearest frame with halves going away from zero, which is what keeps a position
/// and the audio around it in the same place: the stream itself is cut to this, so a mark scaled
/// by it lands where the samples it marks did. Both sides are computed wide enough that only a
/// count no recording could reach saturates.
fn at_48k(frames: u64, rate: NonZeroU32) -> u64 {
    let rate = u128::from(rate.get());
    let scaled = (2 * u128::from(frames) * u128::from(RATE) + rate) / (2 * rate);

    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Why a source could not be brought to the one shape everything behind this stage works in.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PcmError {
    /// The source could not decode the audio it was reading.
    #[error("decode failed")]
    Decode(#[from] DecodeError),
    /// The source states a channel count this stage has no answer for: none at all, or more than
    /// the two a TAF carries.
    #[error("unsupported channel count {0}")]
    UnsupportedChannels(u16),
    /// The source states a sample rate outside the band this stage resamples from, which is
    /// everything between a tenth and ten times the 48 kHz it resamples to.
    #[error("unsupported sample rate {0}")]
    UnsupportedRate(u32),
    /// The resampler refused a turn. Every turn is a chunk of exactly the shape it was built for,
    /// so nothing a source hands out reaches this.
    #[error("resampler failed")]
    Resample(#[source] rubato::ResampleError),
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{at_48k, NonZeroU32, Through, CHANNELS};

    /// A rate as the function under test takes it.
    fn rate(rate: u32) -> NonZeroU32 {
        NonZeroU32::new(rate).unwrap()
    }

    #[test]
    fn a_second_of_the_source_is_a_second_of_the_output() {
        assert_eq!(at_48k(44_100, rate(44_100)), 48_000);
        assert_eq!(at_48k(22_050, rate(22_050)), 48_000);
        assert_eq!(at_48k(96_000, rate(96_000)), 48_000);
    }

    #[test]
    fn a_source_already_at_48_khz_is_not_moved() {
        for frames in [0, 1, 4_095, 123_456_789] {
            assert_eq!(at_48k(frames, rate(48_000)), frames);
        }
    }

    #[test]
    fn a_position_between_two_frames_goes_to_the_nearer_one_and_halves_go_away_from_zero() {
        // At 96 kHz every second source frame lands exactly between two of the output's: 0.5, 1.0,
        // 1.5, 2.0 and 2.5 frames in.
        assert_eq!(at_48k(1, rate(96_000)), 1);
        assert_eq!(at_48k(2, rate(96_000)), 1);
        assert_eq!(at_48k(3, rate(96_000)), 2);
        assert_eq!(at_48k(4, rate(96_000)), 2);
        assert_eq!(at_48k(5, rate(96_000)), 3);
        // At 44 100 the fractions are finer: 1.088, 2.177, 3.265 and 4.354 frames in.
        assert_eq!(at_48k(1, rate(44_100)), 1);
        assert_eq!(at_48k(2, rate(44_100)), 2);
        assert_eq!(at_48k(3, rate(44_100)), 3);
        assert_eq!(at_48k(4, rate(44_100)), 4);
        // And a rate with no half to it rounds on the whole one rather than on a half rounded
        // itself: 0.49999 and 0.99999 of a frame in.
        assert_eq!(at_48k(1, rate(96_001)), 0);
        assert_eq!(at_48k(2, rate(96_001)), 1);
    }

    #[test]
    fn a_position_no_recording_could_reach_is_pinned_at_what_fits() {
        assert_eq!(at_48k(u64::MAX, rate(44_100)), u64::MAX);
        // Slower than the output by the whole band, which is where the scaling is widest.
        assert_eq!(at_48k(u64::MAX, rate(4_800)), u64::MAX);
        // And faster than it, where a tenth of what does not fit still does — with the half of a
        // frame the division leaves rounded up like every other.
        assert_eq!(at_48k(u64::MAX, rate(480_000)), u64::MAX / 10 + 1);
    }

    #[test]
    fn a_block_of_whole_frames_is_handed_on_as_it_came() {
        let mut through = Through::over(CHANNELS);

        let block = through.push(vec![1, 2, 3, 4]);

        assert_eq!(block, [1, 2, 3, 4]);
        assert!(through.carry.is_empty());
    }

    #[test]
    fn a_block_that_ends_mid_frame_hands_the_frame_on_to_the_block_behind_it() {
        let mut through = Through::over(CHANNELS);

        let first = through.push(vec![1, 2, 3]);
        let second = through.push(vec![4, 5]);
        let third = through.push(vec![6, 7]);

        assert_eq!(first, [1, 2]);
        assert_eq!(second, [3, 4]);
        assert_eq!(third, [5, 6]);
        assert_eq!(through.carry, [7]);
    }

    #[test]
    fn one_channel_is_handed_out_on_both_sides() {
        let mut through = Through::over(1);

        let block = through.push(vec![1, 2, 3]);

        assert_eq!(block, [1, 1, 2, 2, 3, 3]);
    }
}
