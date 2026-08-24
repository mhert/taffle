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

/// How long the audio in [`fixtures::VIDEO_FIRST_MP4`] states it plays, to the nanosecond.
///
/// The same clockwork as [`M4B_STATED`] over the second of mono this fixture's audio track carries:
/// [`fixtures::SAMPLE_RATE`] frames of tone with one [`fixtures::AAC_FRAME`] of priming in front of
/// them, which symphonia's MP4 demuxer counts rather than trimming. Which is 45 124 / 44 100
/// seconds — 1 second and 23 219 954 nanoseconds, with the same 28 600 that do not divide out
/// dropped.
const VIDEO_FIRST_STATED: Duration = Duration::new(1, 23_219_954);

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
fn a_track_of_video_ahead_of_the_audio_is_not_the_one_a_length_is_stated_from() {
    let duration = probe_duration(Box::new(Cursor::new(fixtures::VIDEO_FIRST_MP4)))
        .expect("the audio track states a length");

    // This container leads with a track of video and carries its audio behind it, and a conversion
    // takes the first track it has a decoder for. Counting the frames of the container's first
    // track instead would state no length at all for a file that converts perfectly well.
    assert_eq!(duration, VIDEO_FIRST_STATED);
}

#[test]
fn a_container_of_no_audio_at_all_states_no_length() {
    // An MP4 of one video track and nothing else: no track in it is a recording, so there is
    // nothing here whose frames a length could be counted from — the same file a conversion finds
    // nothing to decode in.
    let error = probe_duration(Box::new(Cursor::new(fixtures::NO_AUDIO_MP4)))
        .expect_err("no track of audio");

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
