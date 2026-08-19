//! What [`open_source`] makes of an input: the stream it reports, the samples it hands out, and
//! what it says about bytes it cannot make sense of.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod fixtures;

use std::io::{Cursor, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;
use taf_encode::{open_source, AudioSource, DecodeError, SourceSpec};

/// The WAV fixture, opened.
fn wav_source() -> Box<dyn AudioSource> {
    open_source(Box::new(Cursor::new(fixtures::sine_wav()))).unwrap()
}

/// Every block the fixture decodes to, in the order it decodes to them.
///
/// The walk is bounded by the most blocks the fixture could possibly be split into — one frame
/// each. A source that never says it is done would otherwise hang the run rather than fail it.
fn blocks_of(source: &mut dyn AudioSource) -> Vec<Vec<i16>> {
    let mut blocks: Vec<Vec<i16>> = Vec::new();
    while let Some(block) = source.next_block().unwrap() {
        assert!(
            blocks.len() < fixtures::SAMPLES,
            "the source kept handing out blocks past the end of the fixture"
        );
        blocks.push(block);
    }
    blocks
}

/// The loudest sample of one channel of an interleaved block.
fn peak_of(block: &[i16], channel: usize) -> i32 {
    block
        .iter()
        .skip(channel)
        .step_by(usize::from(fixtures::CHANNELS))
        .map(|sample| i32::from(*sample).abs())
        .max()
        .unwrap()
}

#[test]
fn reports_the_rate_and_channels_the_container_declares() {
    let source = wav_source();

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::SAMPLE_RATE,
            channels: fixtures::CHANNELS,
        }
    );
}

#[test]
fn hands_out_every_sample_of_the_stream_and_no_more() {
    let mut source = wav_source();

    let blocks = blocks_of(source.as_mut());

    let samples: usize = blocks.iter().map(Vec::len).sum();
    assert_eq!(samples, fixtures::SAMPLES);
}

#[test]
fn blocks_end_on_a_frame_boundary() {
    let mut source = wav_source();

    let blocks = blocks_of(source.as_mut());

    for block in &blocks {
        assert_eq!(
            block.len() % usize::from(fixtures::CHANNELS),
            0,
            "a block of {} samples splits a frame",
            block.len()
        );
    }
}

#[test]
fn keeps_the_amplitude_and_the_channel_order_of_the_source() {
    let mut source = wav_source();

    let block = source.next_block().unwrap().expect("the stream has audio");

    // Within a block the sine passes its peak many times over, so both channels come within a
    // percent of theirs — and stay far enough apart that a swap would be plain.
    let left = peak_of(&block, 0);
    let right = peak_of(&block, 1);
    let (left_peak, right_peak) = (
        i32::from(fixtures::LEFT_PEAK),
        i32::from(fixtures::RIGHT_PEAK),
    );

    assert!(
        (left_peak - left_peak / 100..=left_peak).contains(&left),
        "left channel peaks at {left}, not near {left_peak}"
    );
    assert!(
        (right_peak - right_peak / 100..=right_peak).contains(&right),
        "right channel peaks at {right}, not near {right_peak}"
    );
}

#[test]
fn a_wav_carries_no_chapters_and_no_cover() {
    let mut source = wav_source();

    let metadata = source.metadata();

    assert!(metadata.chapters.is_empty());
    assert!(metadata.cover.is_none());
}

#[test]
fn nothing_at_all_is_not_a_format() {
    let opened = open_source(Box::new(Cursor::new(Vec::new())));

    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "empty input opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn bytes_of_no_format_are_not_a_format() {
    let garbage = b"this is not audio and it never was ".repeat(64);

    let opened = open_source(Box::new(Cursor::new(garbage)));

    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "garbage opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn an_input_that_runs_out_mid_header_never_was_a_format() {
    // The fixture's first 20 bytes: enough to be recognized as a WAV, nothing behind the header of
    // the chunk that describes it.
    let truncated = fixtures::sine_wav().get(..20).map(<[u8]>::to_vec).unwrap();

    let opened = open_source(Box::new(Cursor::new(truncated)));

    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "half a header opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn an_input_that_stops_being_readable_mid_header_reports_the_read_failure() {
    // Enough of the fixture to be recognized as a WAV, not enough to finish reading its header.
    let opened = open_source(Box::new(FailingSource::new(fixtures::sine_wav(), 20)));

    assert!(
        matches!(opened, Err(DecodeError::Io(_))),
        "an input that broke off opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn audio_that_does_not_decode_is_not_a_clean_end() {
    let mut source =
        open_source(Box::new(Cursor::new(fixtures::broken_adpcm_wav()))).expect("a WAV opens");

    let decoded = source.next_block();

    assert!(
        matches!(decoded, Err(DecodeError::Decode(_))),
        "undecodable audio came back as {:?}",
        decoded.map(|block| block.map(|samples| samples.len()))
    );
}

#[test]
fn an_input_that_stops_being_readable_mid_stream_is_not_a_clean_end() {
    // Enough of the fixture to describe the stream, nowhere near enough to decode all of it.
    let mut source =
        open_source(Box::new(FailingSource::new(fixtures::sine_wav(), 512))).expect("a WAV opens");

    let mut failure = None;
    for _ in 0..fixtures::SAMPLES {
        match source.next_block() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the truncated input ended as if it were whole"),
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    let failure = failure.expect("the truncated input kept handing out blocks");

    assert!(
        matches!(failure, DecodeError::Io(_)),
        "reported as {failure:?}"
    );
}

/// An input that hands out the first `readable` of its bytes and fails on every read behind them:
/// what a file that goes away mid-decode looks like from here.
struct FailingSource {
    bytes: Cursor<Vec<u8>>,
    readable: u64,
}

impl FailingSource {
    fn new(bytes: Vec<u8>, readable: u64) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            readable,
        }
    }
}

impl Read for FailingSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.readable.saturating_sub(self.bytes.position());
        if left == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the input went away",
            ));
        }
        let take = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        self.bytes.read(&mut buf[..take])
    }
}

impl Seek for FailingSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(pos)
    }
}

impl MediaSource for FailingSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}
