//! What [`probe_duration`] makes of an input: the length the container states about itself, and
//! what it says about bytes that state none.

#![allow(clippy::expect_used)]

// Shared with the decode, pcm and end-to-end tests; the probe reads the encoded fixtures alone.
#[allow(dead_code)]
mod fixtures;

use std::io::Cursor;
use std::time::Duration;

use taf_encode::{probe_duration, ProbeError};

/// How long [`fixtures::TINY_M4B`] states it plays, to the nanosecond.
///
/// The container counts 442 024 frames at [`fixtures::SAMPLE_RATE`]: the
/// [`fixtures::ENCODED_FRAMES`] the tone was authored with, plus one [`fixtures::AAC_FRAME`] of
/// priming in front of it, which symphonia's MP4 demuxer counts rather than trimming. Which is
/// 442 024 / 44 100 seconds — 10 seconds and 23 219 954 nanoseconds, with the 28 600 that do not
/// divide out dropped.
const M4B_STATED: Duration = Duration::new(10, 23_219_954);

#[test]
fn the_probe_states_the_length_the_container_states() {
    let duration =
        probe_duration(Box::new(Cursor::new(fixtures::TINY_M4B))).expect("a stated duration");

    // The container states its length exactly, and so does this: a frontend drawing a percent bar
    // from the number is drawing it from what the file says and not from a rounding of it.
    assert_eq!(duration, M4B_STATED);
}

#[test]
fn an_mp3_states_the_tone_without_the_padding_its_encoder_added() {
    let duration =
        probe_duration(Box::new(Cursor::new(fixtures::TINY_MP3))).expect("a stated duration");

    // The MP3 counts 442 368 frames: the 441 000 of tone it was authored with and 1 368 its
    // encoder needed to get going and to fill its last block. A decoder hands out the tone alone,
    // and this states the tone alone — so the length shown before a conversion and the length that
    // comes out of one are a length of the same audio.
    assert_eq!(duration, Duration::from_secs(10));
}

#[test]
fn a_container_whose_first_track_is_no_recording_states_no_length() {
    // A container states a rate for the audio it carries and for nothing else, and the track this
    // one leads with is video: there is nothing in front to count frames at, and the probe says
    // so rather than digging for a track further back.
    let error = probe_duration(Box::new(Cursor::new(fixtures::VIDEO_FIRST_MP4)))
        .expect_err("the track it leads with states no rate");

    assert!(matches!(error, ProbeError::NoDuration), "{error:?}");
    assert_eq!(error.to_string(), "the container states no duration");
}

#[test]
fn bytes_that_are_no_container_are_refused() {
    let junk = Cursor::new(vec![0u8; 64]);

    let error = probe_duration(Box::new(junk)).expect_err("no container");

    assert!(matches!(error, ProbeError::Unrecognized), "{error:?}");
    assert_eq!(
        error.to_string(),
        "the input is not a format this build recognizes"
    );
}
