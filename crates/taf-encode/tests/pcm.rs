//! What [`Pcm48`] makes of a source: the 48 kHz stereo interleaved `i16` that everything behind
//! this stage — the silence operations, the Opus encoder — is written for, whatever the input was
//! authored as.
//!
//! The sources here are the fixtures the decode tests run on, plus a few built out of samples on
//! the spot: [`AudioSource`] is a public trait, so a stage over one has to hold up for a source
//! nothing in this crate decoded.

// Every cast below is on a count a test states or on a wave bounded by the peak it was scaled
// with, and every index is into a stream the test built or the stage just handed out.
#![allow(
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

// Shared with `decode.rs`, which is what most of it is for: this file uses the WAV builder, the
// Ogg-Opus fixture and the MP4 whose audio is mono.
#[allow(dead_code)]
mod fixtures;

use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::io::Cursor;

use taf_encode::{
    open_source, AudioSource, DecodeError, Pcm48, PcmError, SourceMetadata, SourceSpec,
};

/// The rate every block a stage hands out is in.
const RATE: u32 = 48_000;

/// The channel count every block a stage hands out is in.
const CHANNELS: u16 = 2;

/// How many blocks a walk takes before it calls a stream endless, which no source here comes near.
const MOST_BLOCKS: usize = 100_000;

/// The WAV fixture, opened: two seconds of a 440 Hz sine at 44 100 Hz, stereo.
fn wav_source() -> Box<dyn AudioSource> {
    open_source(Box::new(Cursor::new(fixtures::sine_wav()))).unwrap()
}

/// One of the committed fixtures, opened.
fn source_of(fixture: &'static [u8]) -> Box<dyn AudioSource> {
    open_source(Box::new(Cursor::new(fixture))).unwrap()
}

/// Every block a stage hands out, in the order it hands them out.
fn blocks_of(pcm: &mut Pcm48) -> Vec<Vec<i16>> {
    let mut blocks = Vec::new();
    for _ in 0..MOST_BLOCKS {
        let Some(block) = pcm.next_block().unwrap() else {
            return blocks;
        };
        blocks.push(block);
    }

    panic!("the stage kept handing out blocks");
}

/// Every block a source hands out, in the order it hands them out.
fn source_blocks(source: &mut dyn AudioSource) -> Vec<Vec<i16>> {
    let mut blocks = Vec::new();
    for _ in 0..MOST_BLOCKS {
        let Some(block) = source.next_block().unwrap() else {
            return blocks;
        };
        blocks.push(block);
    }

    panic!("the source kept handing out blocks");
}

/// Every sample a stage hands out, one block behind the other.
fn stream_of(pcm: &mut Pcm48) -> Vec<i16> {
    blocks_of(pcm).concat()
}

/// How many frames a source hands out over the whole of it, at the channels it states.
fn frames_decoded(fixture: &'static [u8]) -> u64 {
    let mut source = source_of(fixture);
    let channels = usize::from(source.spec().channels);
    let samples: usize = source_blocks(source.as_mut()).iter().map(Vec::len).sum();

    u64::try_from(samples / channels).unwrap()
}

/// How many frames a stream of interleaved stereo holds.
fn frames_in(stream: &[i16]) -> u64 {
    u64::try_from(stream.len() / usize::from(CHANNELS)).unwrap()
}

/// The loudest sample one channel of a stream reaches.
fn peak_of(stream: &[i16], channel: usize) -> i32 {
    stream
        .iter()
        .skip(channel)
        .step_by(usize::from(CHANNELS))
        .map(|sample| i32::from(*sample).abs())
        .max()
        .unwrap()
}

/// How often one channel of a stream crosses zero, which is twice the tone's frequency per second.
fn zero_crossings(stream: &[i16], channel: usize) -> usize {
    let samples: Vec<i16> = stream
        .iter()
        .skip(channel)
        .step_by(usize::from(CHANNELS))
        .copied()
        .collect();

    samples
        .windows(2)
        .filter(|pair| (pair[0] < 0) != (pair[1] < 0))
        .count()
}

/// A 440 Hz sine of `frames` frames at `rate`, interleaved over `channels` channels that all carry
/// the same wave at `peak`.
fn tone(frames: u32, rate: u32, channels: u16, peak: i16) -> Vec<i16> {
    let mut samples = Vec::with_capacity(frames as usize * usize::from(channels));
    for frame in 0..frames {
        let value = ((TAU * fixtures::TONE_HZ * f64::from(frame) / f64::from(rate)).sin()
            * f64::from(peak))
        .round() as i16;
        samples.extend(std::iter::repeat_n(value, usize::from(channels)));
    }

    samples
}

/// A square wave of `frames` frames whose halves are `half` frames long, at full scale on both
/// channels: the one signal whose every edge asks the filter behind a resampler to overshoot.
fn square(frames: u32, half: usize) -> Vec<i16> {
    let mut samples = Vec::with_capacity(frames as usize * usize::from(CHANNELS));
    for frame in 0..frames as usize {
        let value = if (frame / half).is_multiple_of(2) {
            i16::MAX
        } else {
            i16::MIN
        };
        samples.extend([value, value]);
    }

    samples
}

/// The shape a source states.
fn spec(sample_rate: u32, channels: u16) -> SourceSpec {
    SourceSpec {
        sample_rate,
        channels,
    }
}

/// A source of exactly the blocks it was built with, at exactly the shape it was built with: what
/// a caller outside this crate can hand a stage.
struct Blocks {
    spec: SourceSpec,
    blocks: VecDeque<Vec<i16>>,
}

impl Blocks {
    /// A source of `samples`, handed out in blocks of `block` samples each and the last one
    /// shorter.
    fn of(spec: SourceSpec, samples: &[i16], block: usize) -> Box<dyn AudioSource> {
        Box::new(Self {
            spec,
            blocks: samples.chunks(block.max(1)).map(<[i16]>::to_vec).collect(),
        })
    }
}

impl AudioSource for Blocks {
    fn spec(&self) -> SourceSpec {
        self.spec
    }

    fn metadata(&mut self) -> SourceMetadata {
        SourceMetadata::default()
    }

    fn next_block(&mut self) -> Result<Option<Vec<i16>>, DecodeError> {
        Ok(self.blocks.pop_front())
    }
}

#[test]
fn a_source_at_another_rate_comes_out_as_long_as_scale_samples_says_it_is() {
    let mut pcm = Pcm48::new(wav_source()).unwrap();

    let stream = stream_of(&mut pcm);

    // Two seconds at 44 100 Hz are two seconds at 48 000: 96 000 frames, exactly — the stage hands
    // out what `scale_samples` states the source amounts to and not a frame more or less.
    assert_eq!(pcm.scale_samples(u64::from(fixtures::FRAMES)), 96_000);
    assert_eq!(frames_in(&stream), 96_000);
}

#[test]
fn every_block_of_a_resampled_stream_is_whole_frames() {
    let mut pcm = Pcm48::new(wav_source()).unwrap();

    let blocks = blocks_of(&mut pcm);

    for block in &blocks {
        assert_eq!(
            block.len() % usize::from(CHANNELS),
            0,
            "a block of {} samples splits a frame",
            block.len()
        );
        assert!(!block.is_empty(), "the stage handed out an empty block");
    }
}

#[test]
fn the_tone_comes_out_at_the_pitch_it_went_in_at() {
    let mut pcm = Pcm48::new(wav_source()).unwrap();

    let stream = stream_of(&mut pcm);

    // The middle second of the stream, which is away from both ends and every transient a filter
    // has at one: a 440 Hz sine crosses zero 880 times a second, and a resampling that changed the
    // rate without changing the wave leaves that where it was.
    let middle = &stream[24_000 * usize::from(CHANNELS)..72_000 * usize::from(CHANNELS)];
    let crossings = zero_crossings(middle, 0);
    assert!(
        (871..=889).contains(&crossings),
        "the tone crosses zero {crossings} times a second, not within a percent of 880"
    );
}

#[test]
fn the_two_channels_come_out_at_the_amplitudes_and_the_order_they_went_in_at() {
    let mut pcm = Pcm48::new(wav_source()).unwrap();

    let stream = stream_of(&mut pcm);

    // The fixture's channels carry the same wave at peaks that are a factor of two apart, so a
    // stage that swapped them, mixed them or scaled one of them lands outside these.
    let left = peak_of(&stream, 0);
    let right = peak_of(&stream, 1);
    let (left_peak, right_peak) = (
        i32::from(fixtures::LEFT_PEAK),
        i32::from(fixtures::RIGHT_PEAK),
    );
    assert!(
        (left_peak - left_peak / 100..=left_peak + left_peak / 100).contains(&left),
        "the left channel peaks at {left}, not within a percent of {left_peak}"
    );
    assert!(
        (right_peak - right_peak / 100..=right_peak + right_peak / 100).contains(&right),
        "the right channel peaks at {right}, not within a percent of {right_peak}"
    );
}

#[test]
fn a_source_already_at_48_khz_stereo_is_handed_through_untouched() {
    let handed_out = source_blocks(source_of(fixtures::TINY_OPUS).as_mut());
    let mut pcm = Pcm48::new(source_of(fixtures::TINY_OPUS)).unwrap();

    let blocks = blocks_of(&mut pcm);

    // Block for block and sample for sample: a stream that is already what this stage is for goes
    // past the resampler rather than through it, so nothing about it can change.
    assert_eq!(blocks, handed_out);
    assert_eq!(frames_in(&blocks.concat()), fixtures::OPUS_FRAMES);
}

#[test]
fn a_mono_source_comes_out_on_both_sides() {
    // The MP4 whose audio is a second of mono AAC at 44 100 Hz: a source that is mono *and* at
    // another rate, so the channel it is missing and the rate it is not at are settled together.
    let frames = frames_decoded(fixtures::VIDEO_FIRST_MP4);
    let mut pcm = Pcm48::new(source_of(fixtures::VIDEO_FIRST_MP4)).unwrap();

    let stream = stream_of(&mut pcm);

    assert_eq!(frames_in(&stream), pcm.scale_samples(frames));
    for (at, frame) in stream.chunks_exact(usize::from(CHANNELS)).enumerate() {
        assert_eq!(
            frame[0], frame[1],
            "frame {at} came out of one channel as two different samples"
        );
    }
}

#[test]
fn a_mono_source_at_48_khz_is_upmixed_without_being_resampled() {
    let mono = tone(4_800, RATE, 1, 12_000);
    let mut pcm = Pcm48::new(Blocks::of(spec(RATE, 1), &mono, 480)).unwrap();

    let stream = stream_of(&mut pcm);

    // Nothing is resampled here, so the upmix is exact: every source sample on both sides, sample
    // for sample and in order.
    assert_eq!(frames_in(&stream), u64::try_from(mono.len()).unwrap());
    for (at, (frame, sample)) in stream
        .chunks_exact(usize::from(CHANNELS))
        .zip(&mono)
        .enumerate()
    {
        assert_eq!(frame, [*sample, *sample], "frame {at}");
    }
}

#[test]
fn scale_samples_states_where_a_source_position_lands_at_48_khz() {
    let at_44_100 = Pcm48::new(wav_source()).unwrap();
    let at_48_000 = Pcm48::new(source_of(fixtures::TINY_OPUS)).unwrap();
    let at_96_000 = Pcm48::new(Blocks::of(spec(96_000, CHANNELS), &[], 1)).unwrap();

    // A second of the source is a second of the output, whatever either counts a second in.
    assert_eq!(at_44_100.scale_samples(44_100), 48_000);
    assert_eq!(at_44_100.scale_samples(0), 0);
    // A source that is already at the output's rate is not moved at all.
    assert_eq!(at_48_000.scale_samples(12_345), 12_345);
    // Halves go away from zero: 0.5, 1.5 and 2.5 frames land on 1, 2 and 3.
    assert_eq!(at_96_000.scale_samples(1), 1);
    assert_eq!(at_96_000.scale_samples(3), 2);
    assert_eq!(at_96_000.scale_samples(5), 3);
    // And a position no recording could reach is pinned at what fits rather than wrapping.
    assert_eq!(at_44_100.scale_samples(u64::MAX), u64::MAX);
}

#[test]
fn a_source_of_more_channels_than_a_taf_carries_is_refused() {
    for channels in [3_u16, 6, u16::MAX] {
        let opened = Pcm48::new(Blocks::of(spec(RATE, channels), &[0; 12], 12));

        assert!(
            matches!(opened, Err(PcmError::UnsupportedChannels(stated)) if stated == channels),
            "a source of {channels} channels opened as {:?}",
            opened.map(|_| "a stage")
        );
    }
}

#[test]
fn a_source_of_no_channels_at_all_is_refused_the_same_way() {
    let opened = Pcm48::new(Blocks::of(spec(RATE, 0), &[], 1));

    assert!(
        matches!(opened, Err(PcmError::UnsupportedChannels(0))),
        "a source of no channels opened as {:?}",
        opened.map(|_| "a stage")
    );
}

#[test]
fn a_source_that_states_no_rate_at_all_is_refused() {
    let opened = Pcm48::new(Blocks::of(spec(0, CHANNELS), &[0; 12], 12));

    assert!(
        matches!(opened, Err(PcmError::UnsupportedRate(0))),
        "a source of no rate opened as {:?}",
        opened.map(|_| "a stage")
    );
}

#[test]
fn a_full_scale_signal_is_clamped_where_the_filter_overshoots_it() {
    // A square wave at full scale: the filter rings around every plateau of it, and a plateau that
    // is already at full scale has nowhere for that ringing to go — so every plateau asks for more
    // than an `i16` states, all the way along it, which is where a conversion that wrapped instead
    // of clamping would put the opposite extreme. 100 Hz at 44 100 Hz is 441 frames to the period.
    const HALF: usize = 441 / 2;
    // How far from an edge a frame has to be to be inside the plateau rather than in the turn.
    const INSIDE: usize = 32;
    let edges = square(fixtures::SAMPLE_RATE, HALF);
    let mut pcm = Pcm48::new(Blocks::of(
        spec(fixtures::SAMPLE_RATE, CHANNELS),
        &edges,
        4_096,
    ))
    .unwrap();

    let stream = stream_of(&mut pcm);

    // Both extremes survive, which they only do when what overshot them was clamped to them.
    assert_eq!(stream.iter().copied().max(), Some(i16::MAX));
    assert_eq!(stream.iter().copied().min(), Some(i16::MIN));
    // And every frame inside a plateau carries that plateau, at full scale and on its own side of
    // zero — 4 401 of them here, none of which came out as the opposite extreme.
    let mut inside = 0;
    for (at, frame) in stream.chunks_exact(usize::from(CHANNELS)).enumerate() {
        let source = at * fixtures::SAMPLE_RATE as usize / RATE as usize;
        let into_half = source % HALF;
        if !(INSIDE..HALF - INSIDE).contains(&into_half) {
            continue;
        }
        inside += 1;
        let positive = (source / HALF).is_multiple_of(2);
        assert_eq!(
            frame[0] > 30_000,
            positive,
            "frame {at} of the plateau at source frame {source} carries {frame:?}"
        );
        assert_eq!(
            frame[0] < -30_000,
            !positive,
            "frame {at} carries {frame:?}"
        );
    }
    assert!(
        inside > 4_000,
        "only {inside} frames landed inside a plateau"
    );
}

#[test]
fn an_impulse_lands_where_the_resampling_counts_it() {
    // Where the resampling puts a sample is what keeps a chapter mark on the audio it marked, and
    // an impulse is the one signal whose place in a stream is a single frame. It is also where the
    // filter's own delay would show: rubato's sinc resampler has already taken that out, and taking
    // it out a second time — which rubato's own procedure for a clip describes — would move the
    // whole stream 139 frames early at 44 100 Hz.
    //
    // The frame it lands on is the source frame *behind* it scaled, one frame back: the resampler
    // counts an output frame from the input it has read past, and the difference that makes is
    // under a fifth of a millisecond anywhere in the band. Everything below is within a frame of
    // it, at rates from one edge of that band to the other.
    const AT: usize = 5_000;

    for rate in [4_800, 8_000, 22_050, fixtures::SAMPLE_RATE, 88_200, 480_000] {
        let mut samples = vec![0; AT * usize::from(CHANNELS)];
        samples.extend([20_000, 20_000]);
        samples.resize(3 * AT * usize::from(CHANNELS), 0);
        let mut pcm = Pcm48::new(Blocks::of(spec(rate, CHANNELS), &samples, 4_096)).unwrap();

        let stream = stream_of(&mut pcm);

        let loudest = u64::try_from(
            stream
                .chunks_exact(usize::from(CHANNELS))
                .enumerate()
                .max_by_key(|(_, frame)| i32::from(frame[0]).abs())
                .expect("the stream has audio")
                .0,
        )
        .unwrap();
        let counted = pcm.scale_samples(u64::try_from(AT).unwrap() + 1) - 1;
        assert!(
            loudest.abs_diff(counted) <= 1,
            "the impulse of a source at {rate} Hz landed at frame {loudest}, not within a frame \
             of {counted}"
        );
        if rate == fixtures::SAMPLE_RATE {
            // At the rate an audiobook is authored at, that is the frame `scale_samples` puts the
            // impulse itself on, with nothing left to round.
            assert_eq!(loudest, pcm.scale_samples(u64::try_from(AT).unwrap()));
        }
    }
}

#[test]
fn a_source_that_ends_on_a_chunk_boundary_still_comes_out_whole() {
    // A source that runs out exactly when the resampler has taken its last full chunk leaves the
    // resampler nothing to be pushed out with — and it is still holding the end of the recording,
    // which the stream would be short of if nothing went in after it.
    const FRAMES: u32 = 4_096;
    let samples = tone(FRAMES, fixtures::SAMPLE_RATE, CHANNELS, 15_000);
    let mut pcm = Pcm48::new(Blocks::of(
        spec(fixtures::SAMPLE_RATE, CHANNELS),
        &samples,
        512,
    ))
    .unwrap();

    let stream = stream_of(&mut pcm);

    assert_eq!(frames_in(&stream), pcm.scale_samples(u64::from(FRAMES)));
    // And the end of it is audio rather than the silence a stream cut short of its last frames
    // would trail off into.
    let last = &stream[stream.len() - 200 * usize::from(CHANNELS)..];
    assert!(
        peak_of(last, 0) > 10_000,
        "the stream trails off into silence"
    );
}

#[test]
fn a_block_that_ends_mid_frame_does_not_take_the_channels_apart() {
    // A source handing out blocks of an odd number of samples: every block ends in the middle of a
    // frame, and the samples behind it belong to the frame the block before it began. The left
    // channel is even here and the right one odd, so a stage that lost track of where a frame
    // begins hands out a frame that is not one.
    let samples: Vec<i16> = (0..2_000).map(|sample| sample * 2 % 30_000).collect();
    let mut interleaved = Vec::new();
    for sample in &samples {
        interleaved.extend([*sample, *sample + 1]);
    }
    let mut pcm = Pcm48::new(Blocks::of(spec(RATE, CHANNELS), &interleaved, 333)).unwrap();

    let stream = stream_of(&mut pcm);

    assert_eq!(stream, interleaved);
    for (at, frame) in stream.chunks_exact(usize::from(CHANNELS)).enumerate() {
        assert_eq!(frame[0] % 2, 0, "frame {at} carries {frame:?}");
        assert_eq!(frame[1] % 2, 1, "frame {at} carries {frame:?}");
    }
}

#[test]
fn a_source_that_ends_mid_frame_loses_the_frame_it_did_not_finish() {
    // Whatever a source handed out that is not a whole frame is not audio anything can play, at
    // either end of the stage: a stream of five samples is two frames.
    let odd = [1_000, 2_000, 3_000, 4_000, 5_000];
    let mut through = Pcm48::new(Blocks::of(spec(RATE, CHANNELS), &odd, 5)).unwrap();
    let mut resampled = Pcm48::new(Blocks::of(spec(24_000, CHANNELS), &odd, 5)).unwrap();

    let through = stream_of(&mut through);
    let resampled = stream_of(&mut resampled);

    assert_eq!(through, [1_000, 2_000, 3_000, 4_000]);
    assert_eq!(frames_in(&resampled), resampled_pcm_frames(2, 24_000));
}

/// How many frames `frames` frames at `rate` amount to at 48 kHz, taken from a stage over a source
/// of that rate rather than restated here.
fn resampled_pcm_frames(frames: u64, rate: u32) -> u64 {
    Pcm48::new(Blocks::of(spec(rate, CHANNELS), &[], 1))
        .unwrap()
        .scale_samples(frames)
}

#[test]
fn a_source_of_no_samples_at_all_comes_out_as_no_stream() {
    for rate in [RATE, fixtures::SAMPLE_RATE] {
        let mut pcm = Pcm48::new(Blocks::of(spec(rate, CHANNELS), &[], 1)).unwrap();

        let blocks = blocks_of(&mut pcm);

        assert!(blocks.is_empty(), "a source of no samples handed out audio");
    }
}

#[test]
fn a_stream_that_has_ended_stays_ended() {
    for rate in [RATE, fixtures::SAMPLE_RATE] {
        let samples = tone(1_000, rate, CHANNELS, 8_000);
        let mut pcm = Pcm48::new(Blocks::of(spec(rate, CHANNELS), &samples, 512)).unwrap();

        blocks_of(&mut pcm);

        assert!(pcm.next_block().unwrap().is_none());
        assert!(pcm.next_block().unwrap().is_none());
    }
}

#[test]
fn a_rate_outside_the_band_this_resamples_from_is_refused() {
    // A tenth of 48 kHz to ten times it: under the 8 kHz the narrowest speech recording is
    // authored at, over the 192 kHz the widest studio one is. What lies outside is not a rate
    // anything was recorded at, and one turn of a resampler for it would be either shorter than
    // its own filter or enormous.
    for rate in [4_799, 480_001, 1, 48_000_000, u32::MAX] {
        let opened = Pcm48::new(Blocks::of(spec(rate, CHANNELS), &[0; 12], 12));

        assert!(
            matches!(opened, Err(PcmError::UnsupportedRate(stated)) if stated == rate),
            "a source at {rate} Hz opened as {:?}",
            opened.map(|_| "a stage")
        );
    }
}

#[test]
fn the_edges_of_that_band_are_resampled_like_any_other_rate() {
    for rate in [4_800_u32, 480_000] {
        let samples = tone(rate / 10, rate, CHANNELS, 10_000);
        let mut pcm = Pcm48::new(Blocks::of(spec(rate, CHANNELS), &samples, 4_096)).unwrap();

        let stream = stream_of(&mut pcm);

        assert_eq!(
            frames_in(&stream),
            pcm.scale_samples(u64::from(rate) / 10),
            "a source at {rate} Hz came out at the wrong length"
        );
    }
}

#[test]
fn a_source_that_fails_to_decode_fails_the_stage_with_it() {
    let broken = open_source(Box::new(Cursor::new(fixtures::broken_adpcm_wav()))).unwrap();
    let mut pcm = Pcm48::new(broken).unwrap();

    let block = pcm.next_block();

    assert!(
        matches!(block, Err(PcmError::Decode(DecodeError::Decode(_)))),
        "undecodable audio came out of the stage as {:?}",
        block.map(|block| block.map(|samples| samples.len()))
    );
}
