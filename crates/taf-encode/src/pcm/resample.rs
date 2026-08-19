//! The resampling itself: rubato's sinc interpolator, the buffers a source's blocks are cut into
//! turns of it, and the conversion between the interleaved `i16` a source decodes to and the
//! planar `f32` a resampler works in.
//!
//! # Which resampler, and what it is set to
//!
//! rubato states two asynchronous resamplers — a polynomial one and a sinc one — and this is the
//! sinc one, [`SincFixedIn`], taking a fixed number of *source* frames per turn. A conversion runs
//! once over a book that is then listened to for hours, so the arithmetic is worth spending: a
//! windowed sinc is what a resampling sounds like when nothing was traded away for speed, and the
//! polynomial one exists for the real-time case this is not.
//!
//! The filter is [`SINC_LEN`] taps through a squared Blackman-Harris window, which is the deepest
//! attenuation of the six windows rubato states and so the least aliasing folded back into a band
//! a listener is in. Its rolloff is the slowest of the six in exchange, and its cutoff is what
//! accounts for that: rubato's own fit of where a window of that length still holds, which for 256
//! taps is 0.947 of the lower of the two Nyquist frequencies — 20.9 kHz for the 44.1 kHz an
//! audiobook is authored at, well above anything speech or the music behind it carries.
//!
//! Between the sinc's pre-computed points — [`OVERSAMPLING`] of them per source frame — the value
//! at the exact position wanted is fitted with a quadratic. The three interpolations rubato states
//! differ in how many pre-computed points they take (two, three, four) and in what they cost; at
//! this many points per frame, a quadratic's error sits some 120 dB down, which is below what an
//! `i16` can carry at all.
//!
//! # A turn, and what it takes
//!
//! [`SincFixedIn`] takes [`CHUNK`] source frames at a time and gives back however many that comes
//! to at 48 kHz. Source blocks are whatever a decoder felt like handing out, so this holds what a
//! turn has not taken yet and takes turns while there is a chunk of it — and at the end of the
//! stream one last turn with zeros behind what is left, which is what pushes the audio still
//! inside the filter out of it.
//!
//! # What is already lined up, and what is cut
//!
//! A filter of [`SINC_LEN`] taps delays what goes through it by half its length, and
//! [`SincFixedIn`] has already taken that out: it starts its window half a filter in front of the
//! first sample, so the frame it gives back first is the frame that went in first. That is what
//! [`Resampler::output_delay`] states — the delay the resampler *has*, not one still to come off —
//! and dropping that many frames, which rubato's own procedure for resampling a clip describes,
//! would move the whole stream 139 frames early at 44.1 kHz. Where an impulse lands is what pins
//! this, in `tests/pcm.rs`.
//!
//! What is cut is the other end: the filter goes on giving back frames after the last sample of
//! the recording has gone in, and those are a window over samples that are not audio. The stream
//! stops at exactly the length the source amounts to.

use std::num::NonZeroU32;

use rubato::{
    calculate_cutoff, Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use super::{at_48k, PcmError, CHANNELS, RATE};

/// How many taps the windowed sinc filter is long, which is rubato's own starting point and what
/// the cutoff below is fitted for.
const SINC_LEN: usize = 256;

/// How many points of the filter are computed between one source frame and the next.
const OVERSAMPLING: usize = 128;

/// The window the sinc is taken through: the deepest attenuation rubato states.
const WINDOW: WindowFunction = WindowFunction::BlackmanHarris2;

/// How many source frames one turn of the resampler takes.
///
/// A chunk this size is around 20 ms of a stream at any rate this resamples from, which keeps the
/// blocks handed out small and the buffers behind them smaller, and it is far longer than the
/// filter reaches — a turn shorter than that would give back nothing at all.
const CHUNK: usize = 1_024;

/// How far the ratio may be moved after the resampler is built, which is not at all: a source
/// states one rate and keeps it, so nothing here ever asks for another ratio.
const FIXED_RATIO: f64 = 1.0;

/// What ±1.0 stands for in the floats the resampler works in: one more than the loudest sample an
/// `i16` states, so that every `i16` is a float and that float is the same `i16` again.
const FULL_SCALE: f32 = 32_768.0;

/// A source at any rate but 48 kHz, resampled a turn at a time.
pub(super) struct Resampler48 {
    /// The resampler itself, built for the source's rate and channels.
    inner: SincFixedIn<f32>,
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
    /// [`PcmError::UnsupportedRate`] when rubato refuses the ratio the rate makes. The only ratios
    /// it refuses are those that are not above zero, and the band `Pcm48::new` takes a rate from
    /// holds none of them.
    pub(super) fn new(rate: NonZeroU32, channels: u16) -> Result<Self, PcmError> {
        let channels = usize::from(channels);
        let parameters = SincInterpolationParameters {
            sinc_len: SINC_LEN,
            f_cutoff: calculate_cutoff(SINC_LEN, WINDOW),
            oversampling_factor: OVERSAMPLING,
            interpolation: SincInterpolationType::Quadratic,
            window: WINDOW,
        };

        let inner = SincFixedIn::new(
            f64::from(RATE) / f64::from(rate.get()),
            FIXED_RATIO,
            parameters,
            CHUNK,
            channels,
        )
        .map_err(|_| PcmError::UnsupportedRate(rate.get()))?;

        Ok(Self {
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
        // inside the filter out of it. Every turn gives back at least one frame — a chunk reaches
        // far past the filter — so the stream is whole in far fewer turns than the frames it is
        // short of, which is what bounds this.
        let short_by =
            whole_stream.saturating_sub(self.frames_out.saturating_add(self.ready_frames()));
        for _ in 0..short_by {
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
    /// # Errors
    ///
    /// [`PcmError::Resample`] as the resampler stated it. Every turn is a chunk of exactly the
    /// shape the resampler was built for, so nothing a source hands out reaches this.
    fn turn(&mut self) -> Result<(), PcmError> {
        let taken = CHUNK.min(self.pending_frames());
        let channels = self.channels;
        let pending = &self.pending;

        for (channel, samples) in self.planar.iter_mut().enumerate() {
            for (frame, sample) in samples.iter_mut().enumerate() {
                *sample = pending
                    .get(frame * channels + channel)
                    .map_or(0.0, |source| from_i16(*source));
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

        for (left, right) in left.iter().zip(right) {
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
