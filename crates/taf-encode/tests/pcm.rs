//! What [`Pcm48`] makes of a source: the 48 kHz stereo interleaved `i16` that everything behind
//! this stage — the silence operations, the Opus encoder — is written for, whatever the input was
//! authored as. And what [`SilenceProcessor`] then makes of *that*: the stream with the silence
//! taken off the front of its chapters, the pauses put in, and the chapter marks moved to where
//! their audio ended up.
//!
//! The sources here are the fixtures the decode tests run on, plus a few built out of samples on
//! the spot: [`AudioSource`] is a public trait, so a stage over one has to hold up for a source
//! nothing in this crate decoded. The silence tests build every stream on the spot and at 48 kHz,
//! so that what a test states about a frame is what the stage under it sees — no resampler in
//! between, and no fixture to read.

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
    open_source, AudioSource, DecodeError, Pcm48, PcmError, SilenceOpts, SilenceProcessor,
    SourceMetadata, SourceSpec, SILENCE_THRESHOLD,
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

/// The same wave on both channels at peaks a factor of two apart — the WAV fixture's own shape, so
/// that a stage which lost track of which side a sample is on is plain to see.
fn two_sided_tone(frames: u32, rate: u32) -> Vec<i16> {
    let left = tone(frames, rate, 1, fixtures::LEFT_PEAK);
    let right = tone(frames, rate, 1, fixtures::RIGHT_PEAK);

    left.iter()
        .zip(&right)
        .flat_map(|(left, right)| [*left, *right])
        .collect()
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

#[test]
fn the_frame_a_source_ended_mid_way_through_never_reaches_the_filter() {
    // Silence with one full-scale sample behind the last whole frame: a source that stopped in the
    // middle of a frame. That sample is not audio — it is one side of a frame that has no other
    // side — and it is as loud as a sample gets, so a resampler that took it in would spread it
    // over a hundred frames of what has to come out silent.
    //
    // The frame count leaves 900 frames over the last whole chunk, which at the slow end of the
    // band is more tail than one turn of the resampler gives back: the frames that are left over
    // stay where they are while the stream is pushed out, so a turn that read past them would read
    // the same sample again at another place in the stream.
    const FRAMES: u32 = 44_932;

    for rate in [fixtures::SAMPLE_RATE, 480_000] {
        let mut samples = vec![0; FRAMES as usize * usize::from(CHANNELS)];
        samples.push(i16::MAX);
        let mut pcm = Pcm48::new(Blocks::of(spec(rate, CHANNELS), &samples, 4_096)).unwrap();

        let stream = stream_of(&mut pcm);

        assert_eq!(frames_in(&stream), pcm.scale_samples(u64::from(FRAMES)));
        let loudest = stream.iter().map(|sample| i32::from(*sample).abs()).max();
        assert_eq!(
            loudest,
            Some(0),
            "the orphan sample of a source at {rate} Hz reached the filter"
        );
    }
}

#[test]
fn where_a_source_cut_its_blocks_does_not_reach_the_resampled_stream() {
    // The same second of tone at 44 100 Hz twice: once in blocks of whole frames, once in blocks
    // of an odd number of samples, so that every one of them ends in the middle of a frame and
    // the frame is finished by the block behind it. A resampler that dropped, repeated or shifted
    // a sample at a seam does not come out the same both ways — and the two channels, a factor of
    // two apart in level, say which side it went wrong on.
    const FRAMES: u32 = 44_100;
    let samples = two_sided_tone(FRAMES, fixtures::SAMPLE_RATE);
    let mut whole = Pcm48::new(Blocks::of(
        spec(fixtures::SAMPLE_RATE, CHANNELS),
        &samples,
        4_096,
    ))
    .unwrap();
    let mut split = Pcm48::new(Blocks::of(
        spec(fixtures::SAMPLE_RATE, CHANNELS),
        &samples,
        333,
    ))
    .unwrap();

    let from_whole_blocks = stream_of(&mut whole);
    let from_split_frames = stream_of(&mut split);

    assert_eq!(from_split_frames, from_whole_blocks);
    assert_eq!(
        frames_in(&from_split_frames),
        split.scale_samples(u64::from(FRAMES))
    );
    // And the channels are still the ones they went in as, at the levels they went in at.
    let left = peak_of(&from_split_frames, 0);
    let right = peak_of(&from_split_frames, 1);
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

// --- the silence stage: what `SilenceProcessor` makes of a stream and of its chapter marks ---

/// How many blocks a walk over a processed stream takes before it calls it endless. The longest
/// stream below is a few hundred blocks; a mutant that hands out silence forever hits this instead
/// of the machine's memory.
const MOST_PROCESSED_BLOCKS: usize = 5_000;

/// A processor over a stream of interleaved 48 kHz stereo, handed to it in blocks of `frames`
/// frames.
fn processor(
    stream: &[i16],
    frames: usize,
    chapters: Vec<u64>,
    opts: SilenceOpts,
) -> SilenceProcessor {
    let source = Blocks::of(spec(RATE, CHANNELS), stream, frames * usize::from(CHANNELS));

    SilenceProcessor::new(Pcm48::new(source).unwrap(), chapters, opts)
}

/// Every block a processor hands out, checked on the way for the two things a block always is:
/// whole frames, and not empty.
fn processed_blocks(silence: &mut SilenceProcessor) -> Vec<Vec<i16>> {
    let mut blocks = Vec::new();
    for _ in 0..MOST_PROCESSED_BLOCKS {
        let Some(block) = silence.next_block().unwrap() else {
            return blocks;
        };
        assert!(!block.is_empty(), "the processor handed out an empty block");
        assert_eq!(
            block.len() % usize::from(CHANNELS),
            0,
            "a block of {} samples splits a frame",
            block.len()
        );
        blocks.push(block);
    }

    panic!("the processor kept handing out blocks");
}

/// Every sample a processor hands out, one block behind the other.
fn processed(silence: &mut SilenceProcessor) -> Vec<i16> {
    processed_blocks(silence).concat()
}

/// Where the chapters of a processed stream landed, which is only there once it has ended.
fn adjusted(silence: &SilenceProcessor) -> Vec<u64> {
    silence
        .adjusted_chapters()
        .expect("the stream has ended")
        .to_vec()
}

/// `frames` frames at `level` on both channels: silence while the level stays below the threshold,
/// and the quietest sound there is once it reaches it.
fn flat(frames: usize, level: i16) -> Vec<i16> {
    vec![level; frames * usize::from(CHANNELS)]
}

/// `frames` frames of sound: every single one of them loud on both sides, at a level twice as high
/// on the left as on the right so that a stage which swapped or folded the channels is plain to
/// see, and alternating in sign so that it is a signal rather than a constant.
fn sound(frames: usize) -> Vec<i16> {
    (0..frames)
        .flat_map(|at| {
            if at % 2 == 0 {
                [12_000, -6_000]
            } else {
                [-12_000, 6_000]
            }
        })
        .collect()
}

/// `frames` frames that say which frame they came from and which side they are on: the left sample
/// counts the frames up from 1 000 and the right one is its negative. Every sample is far above the
/// threshold, so nothing here is ever silence, and a stream that dropped, repeated or swapped a
/// frame says exactly where.
fn ramp(frames: usize) -> Vec<i16> {
    (0..frames)
        .flat_map(|at| {
            let sample = i16::try_from(at % 30_000).unwrap() + 1_000;
            [sample, -sample]
        })
        .collect()
}

/// Where the first frame that is not silence sits, in frames.
fn first_sound(stream: &[i16]) -> Option<usize> {
    stream
        .chunks_exact(usize::from(CHANNELS))
        .position(|frame| {
            frame
                .iter()
                .any(|sample| i32::from(*sample).abs() >= i32::from(SILENCE_THRESHOLD))
        })
}

#[test]
fn the_silence_a_book_begins_with_is_trimmed_off_its_first_chapter() {
    // A second of the noise floor a recording idles at, then a second of audio. The block lengths
    // put the end of that silence at every place a block can end: inside one, exactly on the seam
    // between two, and inside the one block there is.
    let mut stream = flat(48_000, 30);
    stream.extend(sound(48_000));

    for frames in [97, 1_024, 48_000, 96_000] {
        let mut silence = processor(
            &stream,
            frames,
            vec![0],
            SilenceOpts {
                trim_leading: true,
                ..SilenceOpts::default()
            },
        );

        let out = processed(&mut silence);

        assert_eq!(out, sound(48_000), "in blocks of {frames} frames");
        assert_eq!(frames_in(&out), 48_000);
        assert_eq!(adjusted(&silence), [0]);
    }
}

#[test]
fn a_trim_ends_at_the_first_sound_and_leaves_the_silence_behind_it_alone() {
    // Silence, audio, silence, audio — all of it one chapter. What a trim takes off is the silence
    // the chapter *begins* with; the pause the reader took in the middle of it is the recording,
    // and stays. The second stretch of silence is longer than a block, so a trim that went on
    // looking for silence would eat a whole block of it.
    let mut stream = flat(2_400, 20);
    stream.extend(sound(2_400));
    stream.extend(flat(2_400, 20));
    stream.extend(sound(2_400));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0],
        SilenceOpts {
            trim_leading: true,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, [sound(2_400), flat(2_400, 20), sound(2_400)].concat());
    assert_eq!(frames_in(&out), 7_200);
}

#[test]
fn the_leading_skip_drops_exactly_the_frames_it_states_wherever_a_block_ends() {
    let stream = ramp(20_000);

    // 4 800 frames in is inside a block, exactly on the seam between two, and the whole first
    // block — the skip counts frames of the stream and not blocks of it.
    for frames in [700, 1_200, 4_800, 20_000] {
        let mut silence = processor(
            &stream,
            frames,
            vec![0],
            SilenceOpts {
                skip_leading: 4_800,
                ..SilenceOpts::default()
            },
        );

        let out = processed(&mut silence);

        assert_eq!(
            out,
            stream[4_800 * usize::from(CHANNELS)..],
            "in blocks of {frames} frames"
        );
    }

    // And where every block is a single frame, which is where an off-by-one has nowhere to hide.
    let short = ramp(10);
    let mut silence = processor(
        &short,
        1,
        vec![0],
        SilenceOpts {
            skip_leading: 4,
            ..SilenceOpts::default()
        },
    );

    assert_eq!(
        processed(&mut silence),
        short[4 * usize::from(CHANNELS)..].to_vec()
    );
}

#[test]
fn the_leading_skip_runs_in_front_of_the_trim() {
    // Audio, then silence, then audio: the skip takes the first stretch of audio off, which is what
    // leaves the trim silence to work on. A trim that ran first would find sound at the very first
    // frame, take nothing off, and leave that silence sitting in the middle of the output.
    let mut stream = sound(48_000);
    stream.extend(flat(4_800, 20));
    stream.extend(sound(24_000));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0],
        SilenceOpts {
            skip_leading: 48_000,
            trim_leading: true,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, sound(24_000));
    assert_eq!(adjusted(&silence), [0]);
}

#[test]
fn the_pause_a_chapter_is_given_replaces_the_silence_that_was_trimmed_off_it() {
    // The silence the recording came with is at the noise floor rather than at zero, so the second
    // of silence the output begins with can only be the one that was inserted: exact zeros, all
    // 48 000 frames of them.
    let mut stream = flat(48_000, 30);
    stream.extend(sound(24_000));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0],
        SilenceOpts {
            trim_leading: true,
            add_pause_leading: 48_000,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, [flat(48_000, 0), sound(24_000)].concat());
    assert_eq!(frames_in(&out), 72_000);
    assert_eq!(first_sound(&out), Some(48_000));
    assert_eq!(adjusted(&silence), [0]);
}

#[test]
fn a_chapter_that_is_asked_for_it_is_trimmed_wherever_it_begins() {
    // Three chapters, two of which begin with silence: 24 000 frames of audio, then a chapter of
    // 12 000 silent and 24 000 sounding frames, then one of 6 000 silent and 12 000 sounding.
    let mut stream = sound(24_000);
    stream.extend(flat(12_000, 20));
    stream.extend(sound(24_000));
    stream.extend(flat(6_000, 20));
    stream.extend(sound(12_000));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 24_000, 60_000],
        SilenceOpts {
            trim_each_chapter: true,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, [sound(24_000), sound(24_000), sound(12_000)].concat());
    // The second chapter begins where the first one ended, and the third has moved left by both
    // trims together.
    assert_eq!(adjusted(&silence), [0, 24_000, 48_000]);
}

#[test]
fn a_pause_at_every_chapter_moves_every_chapter_behind_it_back() {
    let stream = sound(72_000);
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 24_000, 48_000],
        SilenceOpts {
            add_pause_each_chapter: 4_800,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    // Each chapter is a pause and then its audio, and each mark is on the first frame of its own
    // pause — so the second has moved back by one pause and the third by two.
    assert_eq!(
        out,
        [
            flat(4_800, 0),
            sound(24_000),
            flat(4_800, 0),
            sound(24_000),
            flat(4_800, 0),
            sound(24_000),
        ]
        .concat()
    );
    assert_eq!(adjusted(&silence), [0, 28_800, 57_600]);
    assert_eq!(frames_in(&out), 86_400);
}

#[test]
fn two_chapters_that_begin_in_the_same_place_both_begin_there() {
    // Chapter starts that are not strictly increasing are not this stage's to refuse — the chapter
    // plan is settled in front of it. A second chapter at the same frame as the one before it, and
    // a third whose start lies behind both, begin one after the other at the first frame at or
    // behind them, each with its own pause.
    let stream = sound(9_600);
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 4_800, 4_800, 1_000],
        SilenceOpts {
            add_pause_each_chapter: 1_200,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(
        out,
        [flat(1_200, 0), sound(4_800), flat(3_600, 0), sound(4_800),].concat()
    );
    assert_eq!(adjusted(&silence), [0, 6_000, 7_200, 8_400]);
}

#[test]
fn a_chapter_that_is_nothing_but_silence_trims_to_no_length_at_all() {
    let mut stream = sound(24_000);
    stream.extend(flat(24_000, 20));
    stream.extend(sound(24_000));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 24_000, 48_000],
        SilenceOpts {
            trim_each_chapter: true,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, [sound(24_000), sound(24_000)].concat());
    // The chapter is still there and still in its place in the order — it just begins where the
    // chapter behind it does.
    assert_eq!(adjusted(&silence), [0, 24_000, 24_000]);
}

#[test]
fn a_stream_that_is_nothing_but_silence_comes_out_as_no_stream_at_all() {
    let stream = flat(48_000, 20);
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 24_000],
        SilenceOpts {
            trim_each_chapter: true,
            ..SilenceOpts::default()
        },
    );

    let blocks = processed_blocks(&mut silence);

    assert!(blocks.is_empty(), "a stream of silence handed out audio");
    assert_eq!(adjusted(&silence), [0, 0]);
}

#[test]
fn a_sample_at_the_threshold_is_sound_and_one_below_it_is_silence() {
    assert_eq!(SILENCE_THRESHOLD, 58);

    // Below the threshold on both sides is silence and goes; at it on either side is sound and
    // stays. Both signs of both, since what counts is how far from zero a sample is.
    for level in [SILENCE_THRESHOLD - 1, -(SILENCE_THRESHOLD - 1)] {
        let mut stream = flat(4_800, level);
        stream.extend(sound(4_800));
        let mut silence = processor(
            &stream,
            1_024,
            vec![0],
            SilenceOpts {
                trim_leading: true,
                ..SilenceOpts::default()
            },
        );

        assert_eq!(processed(&mut silence), sound(4_800), "at level {level}");
    }
    for level in [SILENCE_THRESHOLD, -SILENCE_THRESHOLD] {
        let mut stream = flat(4_800, level);
        stream.extend(sound(4_800));
        let mut silence = processor(
            &stream,
            1_024,
            vec![0],
            SilenceOpts {
                trim_leading: true,
                ..SilenceOpts::default()
            },
        );

        assert_eq!(processed(&mut silence), stream, "at level {level}");
    }
}

#[test]
fn a_frame_is_sound_when_either_of_its_channels_is() {
    // One side at a level nobody could miss and the other at zero: the frame is audible, so it is
    // not silence. The frame's peak is the louder of its two samples, not the quieter one and not
    // the two of them averaged.
    for frame in [[100, 0], [0, 100], [-100, 0], [0, -100]] {
        let loud: Vec<i16> = std::iter::repeat_n(frame, 4_800).flatten().collect();
        let mut stream = loud.clone();
        stream.extend(sound(4_800));
        let mut silence = processor(
            &stream,
            1_024,
            vec![0],
            SilenceOpts {
                trim_leading: true,
                ..SilenceOpts::default()
            },
        );

        assert_eq!(processed(&mut silence), stream, "with frames of {frame:?}");
    }

    // And a frame whose loudest side is still below the threshold is silence, whichever side that
    // is.
    for frame in [[SILENCE_THRESHOLD - 1, 0], [0, SILENCE_THRESHOLD - 1]] {
        let quiet: Vec<i16> = std::iter::repeat_n(frame, 4_800).flatten().collect();
        let mut stream = quiet;
        stream.extend(sound(4_800));
        let mut silence = processor(
            &stream,
            1_024,
            vec![0],
            SilenceOpts {
                trim_leading: true,
                ..SilenceOpts::default()
            },
        );

        assert_eq!(
            processed(&mut silence),
            sound(4_800),
            "with frames of {frame:?}"
        );
    }
}

#[test]
fn nothing_asked_for_is_nothing_done() {
    let mut stream = flat(4_096, 20);
    stream.extend(sound(8_192));
    stream.extend(flat(4_096, 20));
    stream.extend(sound(4_096));
    let handed_out = {
        let source = Blocks::of(spec(RATE, CHANNELS), &stream, 1_024 * usize::from(CHANNELS));
        blocks_of(&mut Pcm48::new(source).unwrap())
    };
    // Chapter starts on the seams between blocks, so that a stream nothing is done to comes out in
    // the very blocks it went in as.
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 4_096, 16_384],
        SilenceOpts::default(),
    );

    let blocks = processed_blocks(&mut silence);

    assert_eq!(blocks, handed_out);
    assert_eq!(adjusted(&silence), [0, 4_096, 16_384]);
}

#[test]
fn every_chapter_is_final_before_the_first_frame_at_it_is_handed_out() {
    let mut stream = flat(6_000, 20);
    stream.extend(sound(18_000));
    stream.extend(flat(12_000, 20));
    stream.extend(sound(12_000));
    stream.extend(sound(24_000));
    // Blocks of 700 frames put both chapter starts inside a block rather than on a seam, so the
    // processor has to cut its own blocks where a chapter begins.
    let mut silence = processor(
        &stream,
        700,
        vec![0, 24_000, 48_000],
        SilenceOpts {
            trim_each_chapter: true,
            add_pause_leading: 2_400,
            add_pause_each_chapter: 4_800,
            ..SilenceOpts::default()
        },
    );

    // What was known after every block, against where that block began.
    let mut history: Vec<(u64, Vec<u64>)> = Vec::new();
    let mut emitted = 0;
    for _ in 0..MOST_PROCESSED_BLOCKS {
        assert!(
            silence.adjusted_chapters().is_none(),
            "the offsets were final while the stream was still running"
        );
        let Some(block) = silence.next_block().unwrap() else {
            break;
        };
        let marks = silence.chapters_emitted().to_vec();
        if let Some((_, before)) = history.last() {
            assert!(
                marks.starts_with(before),
                "the chapters known so far are not a prefix of themselves: {before:?} then {marks:?}"
            );
        }
        history.push((emitted, marks));
        emitted += frames_in(&block);
    }

    let marks = adjusted(&silence);
    assert_eq!(marks, [0, 25_200, 42_000]);
    assert_eq!(emitted, 70_800);
    assert_eq!(silence.chapters_emitted(), marks);
    for (start, known) in &history {
        // Every chapter at or in front of a block's first frame was known when that block came
        // out, and no chapter that lies behind it was.
        let due: Vec<u64> = marks.iter().copied().filter(|mark| mark <= start).collect();
        assert_eq!(*known, due, "after the block at frame {start}");
    }
    // And no block spans a chapter start: every mark is the first frame of a block.
    let starts: Vec<u64> = history.iter().map(|(start, _)| *start).collect();
    for mark in &marks {
        assert!(
            starts.contains(mark),
            "the chapter at {mark} sits inside a block instead of at the start of one"
        );
    }
}

#[test]
fn the_skip_the_trim_and_the_pause_leave_exactly_the_pause_in_front_of_the_audio() {
    // A worked example: `--skip-leading 4.4 --trim-pause-leading --add-pause-leading 1.0`
    // yields exactly 1.0 s of silence before the first non-silent sample. 4.4 s of a jingle, then
    // 2 s of the noise floor, then the book.
    const SECOND: usize = 48_000;
    let mut stream = sound(4 * SECOND + SECOND * 2 / 5);
    stream.extend(flat(2 * SECOND, 40));
    stream.extend(sound(10 * SECOND));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0],
        SilenceOpts {
            skip_leading: 4 * 48_000 + 48_000 * 2 / 5,
            trim_leading: true,
            add_pause_leading: 48_000,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(first_sound(&out), Some(SECOND));
    assert_eq!(out, [flat(SECOND, 0), sound(10 * SECOND)].concat());
    assert_eq!(adjusted(&silence), [0]);
}

#[test]
fn a_chapter_the_skip_ran_past_begins_where_the_skip_ended() {
    let stream = ramp(20_000);
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 1_000, 6_000],
        SilenceOpts {
            skip_leading: 5_000,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, stream[5_000 * usize::from(CHANNELS)..]);
    // The two chapters the skip ran past have no frames of their own left, so both begin where the
    // output does; the one behind it keeps its distance from there.
    assert_eq!(adjusted(&silence), [0, 0, 1_000]);
}

#[test]
fn a_stream_shorter_than_the_skip_comes_out_as_no_stream_at_all() {
    let stream = ramp(1_000);
    let mut silence = processor(
        &stream,
        128,
        vec![0, 500],
        SilenceOpts {
            skip_leading: 4_800,
            trim_each_chapter: true,
            add_pause_leading: 4_800,
            add_pause_each_chapter: 4_800,
            ..SilenceOpts::default()
        },
    );

    let blocks = processed_blocks(&mut silence);

    // No chapter ever began, so no pause was ever owed: a stream the skip outran is empty rather
    // than a stretch of silence nobody asked to hear.
    assert!(
        blocks.is_empty(),
        "the skipped-away stream handed out audio"
    );
    assert_eq!(adjusted(&silence), [0, 0]);
}

#[test]
fn a_stream_with_no_chapters_has_nothing_to_trim_and_nowhere_to_put_a_pause() {
    let mut stream = flat(4_800, 20);
    stream.extend(sound(4_800));
    let mut silence = processor(
        &stream,
        1_024,
        Vec::new(),
        SilenceOpts {
            skip_leading: 1_200,
            trim_leading: true,
            trim_each_chapter: true,
            add_pause_leading: 4_800,
            add_pause_each_chapter: 4_800,
        },
    );

    let out = processed(&mut silence);

    // The skip is the one thing that is not a chapter's: it happens either way. Everything else
    // hangs on a chapter start, and there is none.
    assert_eq!(out, stream[1_200 * usize::from(CHANNELS)..]);
    assert!(adjusted(&silence).is_empty());
}

#[test]
fn what_lies_in_front_of_the_first_chapter_is_handed_through_untouched() {
    // A chapter list that does not begin at zero: the frames in front of the first chapter belong
    // to no chapter, so nothing is trimmed off them and no pause goes in front of them.
    let mut stream = flat(2_400, 20);
    stream.extend(flat(2_400, 20));
    stream.extend(sound(4_800));
    let mut silence = processor(
        &stream,
        1_024,
        vec![2_400],
        SilenceOpts {
            trim_leading: true,
            add_pause_leading: 1_200,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(
        out,
        [flat(2_400, 20), flat(1_200, 0), sound(4_800)].concat()
    );
    assert_eq!(adjusted(&silence), [2_400]);
}

#[test]
fn trimming_every_chapter_trims_the_first_one_too() {
    let mut stream = flat(4_800, 20);
    stream.extend(sound(4_800));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0],
        SilenceOpts {
            trim_each_chapter: true,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    assert_eq!(out, sound(4_800));
}

#[test]
fn the_leading_trim_is_the_first_chapters_alone() {
    let mut stream = flat(2_400, 20);
    stream.extend(sound(2_400));
    stream.extend(flat(2_400, 20));
    stream.extend(sound(2_400));
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 4_800],
        SilenceOpts {
            trim_leading: true,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    // The first chapter's silence is gone and the second chapter's is not.
    assert_eq!(out, [sound(2_400), flat(2_400, 20), sound(2_400)].concat());
    assert_eq!(adjusted(&silence), [0, 2_400]);
}

#[test]
fn both_pauses_go_in_at_the_first_chapter() {
    let stream = sound(9_600);
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 4_800],
        SilenceOpts {
            add_pause_leading: 1_200,
            add_pause_each_chapter: 2_400,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    // The first chapter is given both, every other chapter only the one that is every chapter's.
    assert_eq!(
        out,
        [flat(3_600, 0), sound(4_800), flat(2_400, 0), sound(4_800),].concat()
    );
    assert_eq!(adjusted(&silence), [0, 8_400]);
}

#[test]
fn a_pause_is_handed_out_a_tenth_of_a_second_at_a_time() {
    let stream = sound(4_800);
    let mut silence = processor(
        &stream,
        4_800,
        vec![0],
        SilenceOpts {
            add_pause_leading: 48_000,
            ..SilenceOpts::default()
        },
    );

    let blocks = processed_blocks(&mut silence);

    // A second of silence is ten blocks of a tenth of a second, and then the audio: a pause of any
    // length costs one small block at a time rather than one allocation the size of the pause.
    assert_eq!(blocks.len(), 11);
    for block in blocks.iter().take(10) {
        assert_eq!(frames_in(block), 4_800);
    }
    assert_eq!(blocks.concat(), [flat(48_000, 0), sound(4_800)].concat());
}

#[test]
fn a_processed_stream_that_has_ended_stays_ended() {
    let stream = sound(2_400);
    let mut silence = processor(&stream, 1_024, vec![0], SilenceOpts::default());

    processed_blocks(&mut silence);

    assert!(silence.next_block().unwrap().is_none());
    assert!(silence.next_block().unwrap().is_none());
    assert_eq!(adjusted(&silence), [0]);
}

#[test]
fn a_source_of_no_samples_at_all_comes_out_as_no_stream_and_leaves_its_chapters_at_the_end() {
    let mut silence = processor(
        &[],
        1_024,
        vec![0, 4_800],
        SilenceOpts {
            add_pause_leading: 4_800,
            add_pause_each_chapter: 4_800,
            ..SilenceOpts::default()
        },
    );

    let blocks = processed_blocks(&mut silence);

    // A chapter with no frame of its own left to begin on gets no pause: there is nothing there to
    // put a pause in front of.
    assert!(blocks.is_empty(), "a source of no samples handed out audio");
    assert_eq!(adjusted(&silence), [0, 0]);
}

#[test]
fn a_chapter_at_the_end_of_the_stream_lands_at_the_end_of_the_output() {
    let stream = sound(4_800);
    let mut silence = processor(
        &stream,
        1_024,
        vec![0, 4_800, 9_600],
        SilenceOpts {
            add_pause_each_chapter: 1_200,
            ..SilenceOpts::default()
        },
    );

    let out = processed(&mut silence);

    // Only the first chapter has a frame of its own, so only it is given a pause; the two behind
    // the end of the stream land where the output ended.
    assert_eq!(out, [flat(1_200, 0), sound(4_800)].concat());
    assert_eq!(adjusted(&silence), [0, 6_000, 6_000]);
}

#[test]
fn a_source_that_fails_to_decode_fails_the_processor_with_it() {
    let broken = open_source(Box::new(Cursor::new(fixtures::broken_adpcm_wav()))).unwrap();
    let mut silence =
        SilenceProcessor::new(Pcm48::new(broken).unwrap(), vec![0], SilenceOpts::default());

    let block = silence.next_block();

    assert!(
        matches!(block, Err(PcmError::Decode(DecodeError::Decode(_)))),
        "undecodable audio came out of the processor as {:?}",
        block.map(|block| block.map(|samples| samples.len()))
    );
    assert!(silence.adjusted_chapters().is_none());
}
