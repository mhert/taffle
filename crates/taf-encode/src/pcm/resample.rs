//! The resampling itself: rubato's FFT resampler, the buffers a source's blocks are cut into turns
//! of it, and the conversion between the interleaved `i16` a source decodes to and the planar `f32`
//! a resampler works in.
//!
//! # Which resampler, and why this one
//!
//! Resampling to another rate is a lowpass filter, and the one behind this is a sinc taken through
//! a squared Blackman-Harris window: the deepest attenuation rubato states, so that what folds back
//! into a band a listener is in sits far under what an `i16` carries at all. Walking a filter like
//! that over the signal a tap at a time is where a resampling's arithmetic goes, and
//! [`FftFixedIn`] applies it where a convolution is a multiplication instead: it transforms a
//! segment of the signal, multiplies it by the filter's spectrum, transforms back, and adds the
//! tail of each segment into the front of the next. That is the convolution theorem rather than a
//! trade of quality for speed — the same windowed filter at a tenth of the arithmetic, and a
//! conversion is that arithmetic for as long as the book is.
//!
//! # A turn, and what it takes
//!
//! [`FftFixedIn`] takes [`CHUNK`] source frames at a time and gives back however many that comes to
//! at 48 kHz. Source blocks are whatever a decoder felt like handing out, so this holds what a turn
//! has not taken yet and takes turns while there is a chunk of it — and at the end of the stream
//! turns with zeros behind what is left, which is what pushes the audio still inside the filter out
//! of it.
//!
//! # What is lined up, and what is cut
//!
//! The filter delays what goes through it by half its length, and what comes back carries that
//! delay: the frames a turn gives back first are the filter running up on the silence in front of
//! the recording. [`Resampler::output_delay`] states how many frames that is, and this drops
//! exactly that many off the front — the procedure rubato's own documentation describes for
//! resampling a clip — so that the frame handed out first is the frame that went in first. Where an
//! impulse lands is what pins this, in `tests/pcm.rs`.
//!
//! What is cut is the other end: the filter goes on giving back frames after the last sample of the
//! recording has gone in, and those are a window over samples that are not audio. The stream stops
//! at exactly the length the source amounts to.

use std::num::NonZeroU32;

use rubato::{FftFixedIn, Resampler};

use super::{at_48k, PcmError, CHANNELS, RATE};

/// How many source frames one turn of the resampler takes.
///
/// A chunk this size is around 20 ms of a stream at any rate this resamples from, which keeps the
/// blocks handed out small and the buffers behind them smaller. It is also what the segments the
/// filter is applied in are sized from — see [`SUB_CHUNKS`].
const CHUNK: usize = 1_024;

/// How many segments one turn is cut into for the filter to be applied in, which is the number
/// rubato's own example states.
///
/// It is a wish rather than a count: rubato takes a chunk over this many as the length it would
/// like a segment to be and rounds that up to a whole number of the source frames the ratio to
/// 48 kHz repeats over — a second of them where the ratio does not reduce at all. That is why a
/// turn gives nothing back until a segment is full, and why flushing the end of a stream can take
/// many turns before one of them hands anything over.
const SUB_CHUNKS: usize = 2;

/// What ±1.0 stands for in the floats the resampler works in: one more than the loudest sample an
/// `i16` states, so that every `i16` is a float and that float is the same `i16` again.
const FULL_SCALE: f32 = 32_768.0;

/// A source at any rate but 48 kHz, resampled a turn at a time: the filter's leading delay comes
/// off the front of what it gives back, and the stream ends at the length the source amounts to.
pub(super) struct Resampler48 {
    /// The resampler itself, built for the source's rate and channels.
    inner: FftFixedIn<f32>,
    /// How many channels the source interleaves its blocks over: one or two.
    channels: usize,
    /// The rate the source states, which is what a position of its is counted in.
    rate: NonZeroU32,
    /// The source samples no turn has taken yet, interleaved as they arrived.
    pending: Vec<i16>,
    /// What one turn is fed, a buffer per source channel: a resampler works a channel at a time,
    /// and these are the same buffers every turn.
    planar: Vec<Vec<f32>>,
    /// Resampled stereo samples that have not been handed out yet.
    ready: Vec<i16>,
    /// Output frames still to drop: the filter's leading delay, so the first frame out is the
    /// first frame in.
    to_drop: usize,
    /// How many samples the source has handed over.
    samples_in: u64,
    /// How many frames have been handed out.
    frames_out: u64,
}

impl Resampler48 {
    /// A resampler from `rate` to 48 kHz over a source of `channels` channels.
    ///
    /// # Errors
    ///
    /// [`PcmError::UnsupportedRate`] when the rate is not a count this machine holds, or one
    /// rubato refuses — which is a rate of no frames at all. The band `Pcm48::new` takes a rate
    /// from holds neither.
    pub(super) fn new(rate: NonZeroU32, channels: u16) -> Result<Self, PcmError> {
        let channels = usize::from(channels);
        let rate_in =
            usize::try_from(rate.get()).map_err(|_| PcmError::UnsupportedRate(rate.get()))?;
        let inner = FftFixedIn::<f32>::new(rate_in, RATE as usize, CHUNK, SUB_CHUNKS, channels)
            .map_err(|_| PcmError::UnsupportedRate(rate.get()))?;

        Ok(Self {
            to_drop: inner.output_delay(),
            inner,
            channels,
            rate,
            pending: Vec::new(),
            planar: vec![vec![0.0; CHUNK]; channels],
            ready: Vec::new(),
            samples_in: 0,
            frames_out: 0,
        })
    }

    /// What one block of the source amounts to at 48 kHz, which is nothing at all while the
    /// resampler is still short of a chunk.
    ///
    /// # Errors
    ///
    /// [`PcmError::Resample`] as the resampler stated it.
    pub(super) fn push(&mut self, block: &[i16]) -> Result<Vec<i16>, PcmError> {
        self.pending.extend_from_slice(block);
        self.samples_in = self.samples_in.saturating_add(block.len() as u64);

        while self.pending_frames() >= CHUNK {
            self.turn()?;
        }

        let allowed = self.stream_so_far().saturating_sub(self.frames_out);

        Ok(self.hand_out(allowed))
    }

    /// The last of the stream: what is left of the source, and the audio the filter still holds
    /// behind it.
    ///
    /// What comes back brings the stream to exactly the length the source amounts to at 48 kHz —
    /// see [`at_48k`] — however much of the filter's tail is left over.
    ///
    /// # Errors
    ///
    /// [`PcmError::Resample`] as the resampler stated it.
    pub(super) fn finish(&mut self) -> Result<Vec<i16>, PcmError> {
        let whole_stream = self.stream_so_far();

        // The end of a stream is nothing special to a turn: it takes what is left of the source
        // with zeros behind it, and once that is in, more zeros are what push the audio still
        // inside the filter out of it. What ends this is the length the stream reaches, and a turn
        // may well give nothing back on the way there — the filter hands frames back a segment at
        // a time, and a segment can be many chunks of source frames long.
        for _ in 0..self.flush_turns() {
            if self.frames_out.saturating_add(self.ready_frames()) >= whole_stream {
                break;
            }
            self.turn()?;
        }

        Ok(self.hand_out(whole_stream.saturating_sub(self.frames_out)))
    }

    /// One turn of the resampler: a chunk of what is pending, zeros behind it where the source has
    /// run out, and what comes back kept for handing out.
    ///
    /// What goes in is whole frames and no more of them than a turn takes. A source that stopped in
    /// the middle of a frame left a sample that is one side of a frame with no other side: putting
    /// it in would play it against silence on the side it does not have, and — since a turn only
    /// takes whole frames away — put it in again at another place in the stream on the next turn.
    ///
    /// # Errors
    ///
    /// [`PcmError::Resample`] as the resampler stated it. Every turn is a chunk of exactly the
    /// shape the resampler was built for, so nothing a source hands out reaches this.
    fn turn(&mut self) -> Result<(), PcmError> {
        let taken = CHUNK.min(self.pending_frames());
        let channels = self.channels;
        let frames = self.pending.get(..taken * channels).unwrap_or_default();

        for (channel, samples) in self.planar.iter_mut().enumerate() {
            // Zeros first: at the end of the stream a turn is fed less than a chunk, and what the
            // source did not fill is silence behind it.
            samples.fill(0.0);
            for (sample, frame) in samples.iter_mut().zip(frames.chunks_exact(channels)) {
                *sample = frame.get(channel).map_or(0.0, |source| from_i16(*source));
            }
        }

        let produced = self
            .inner
            .process(&self.planar, None)
            .map_err(PcmError::Resample)?;

        self.pending.drain(..taken * channels);
        self.keep(&produced);

        Ok(())
    }

    /// What one turn gave back, in the two channels and the `i16` everything behind this works in.
    fn keep(&mut self, produced: &[Vec<f32>]) {
        let nothing: &[f32] = &[];
        let left = produced.first().map_or(nothing, Vec::as_slice);
        // A mono source was resampled as the one signal it is, and that signal is both sides.
        let right = produced.get(1).map_or(left, Vec::as_slice);

        let dropped = self.to_drop.min(left.len());
        self.to_drop -= dropped;
        for (left, right) in left.iter().zip(right).skip(dropped) {
            self.ready.push(to_i16(*left));
            self.ready.push(to_i16(*right));
        }
    }

    /// `frames` frames of what is ready, or as many of them as there are.
    fn hand_out(&mut self, frames: u64) -> Vec<i16> {
        let frames = usize::try_from(frames)
            .unwrap_or(usize::MAX)
            .min(self.ready.len() / usize::from(CHANNELS));
        self.frames_out = self.frames_out.saturating_add(frames as u64);

        self.ready.drain(..frames * usize::from(CHANNELS)).collect()
    }

    /// How long the stream is as far as the source has been read: what it handed over, at 48 kHz.
    ///
    /// This is what the stage may have handed out by now and, once the source has stated the end
    /// of its stream, exactly how long the whole stream is.
    fn stream_so_far(&self) -> u64 {
        at_48k(self.samples_in / self.channels as u64, self.rate)
    }

    /// How many turns of the resampler put the whole of the recording out of the filter, at the
    /// most.
    ///
    /// A turn gives nothing back until the filter holds a whole segment to transform, and a segment
    /// is at most a second of source frames — see [`SUB_CHUNKS`]. Two seconds of them behind what
    /// is still pending run past the end of the last segment and the delay in front of it at every
    /// rate this resamples from, so a flush of this many turns is a flush of the whole stream. It
    /// ends at the length the stream reaches, which comes long before this.
    fn flush_turns(&self) -> usize {
        let rate = usize::try_from(self.rate.get()).unwrap_or(usize::MAX);

        self.pending_frames()
            .saturating_add(rate.saturating_mul(2))
            .div_ceil(CHUNK)
    }

    /// How many source frames no turn has taken yet. A sample the source handed over that is not a
    /// whole frame stays here, which is where it ends.
    fn pending_frames(&self) -> usize {
        self.pending.len() / self.channels
    }

    /// How many resampled frames are waiting to be handed out.
    fn ready_frames(&self) -> u64 {
        (self.ready.len() / usize::from(CHANNELS)) as u64
    }
}

/// A source sample as the float the resampler works in.
fn from_i16(sample: i16) -> f32 {
    f32::from(sample) / FULL_SCALE
}

/// A resampled sample as the `i16` everything behind this stage works in.
///
/// Rounded to the nearest, halves away from zero, and clamped rather than wrapped where the filter
/// overshot full scale — which it does around every edge in the signal, and which is the one place
/// where the loudest sample there is and the quietest one lie next to each other.
fn to_i16(sample: f32) -> i16 {
    let scaled = (sample * FULL_SCALE).round();

    if scaled >= f32::from(i16::MAX) {
        return i16::MAX;
    }
    if scaled <= f32::from(i16::MIN) {
        return i16::MIN;
    }

    // Everything outside the range came back above, so this neither truncates nor wraps. A float
    // that is not a number at all is no sample, and the cast takes one to silence.
    #[allow(clippy::cast_possible_truncation)]
    let rounded = scaled as i16;

    rounded
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{from_i16, to_i16, FULL_SCALE};

    /// A value of so many samples' worth, as the float the resampler works in.
    fn samples(value: f32) -> f32 {
        value / FULL_SCALE
    }

    #[test]
    fn every_sample_there_is_comes_back_as_itself() {
        for sample in i16::MIN..=i16::MAX {
            assert_eq!(
                to_i16(from_i16(sample)),
                sample,
                "{sample} did not come back"
            );
        }
    }

    #[test]
    fn a_value_between_two_samples_goes_to_the_nearer_one_and_halves_go_away_from_zero() {
        assert_eq!(to_i16(samples(1.4)), 1);
        assert_eq!(to_i16(samples(1.5)), 2);
        assert_eq!(to_i16(samples(1.6)), 2);
        assert_eq!(to_i16(samples(-1.4)), -1);
        assert_eq!(to_i16(samples(-1.5)), -2);
        assert_eq!(to_i16(samples(-1.6)), -2);
        assert_eq!(to_i16(samples(0.5)), 1);
        assert_eq!(to_i16(samples(-0.5)), -1);
    }

    #[test]
    fn a_value_past_full_scale_is_clamped_and_not_wrapped() {
        // What a filter overshooting an edge in a full-scale signal hands back.
        assert_eq!(to_i16(1.2), i16::MAX);
        assert_eq!(to_i16(-1.2), i16::MIN);
        // And what nothing hands back, since every one of them started as an `i16`.
        assert_eq!(to_i16(f32::MAX), i16::MAX);
        assert_eq!(to_i16(f32::MIN), i16::MIN);
        assert_eq!(to_i16(f32::INFINITY), i16::MAX);
        assert_eq!(to_i16(f32::NEG_INFINITY), i16::MIN);
    }

    #[test]
    fn full_scale_itself_is_the_loudest_sample_there_is() {
        // ±1.0 is one step past the loudest positive sample, which is where clamping puts it.
        assert_eq!(to_i16(1.0), i16::MAX);
        assert_eq!(to_i16(-1.0), i16::MIN);
    }

    #[test]
    fn a_float_that_is_not_a_number_is_no_sample_at_all() {
        assert_eq!(to_i16(f32::NAN), 0);
    }
}
