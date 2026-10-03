//! What [`probe_duration`] makes of an input: the length the container states about itself, and
//! what it says about bytes that state none.

#![allow(clippy::expect_used)]

// Shared with the decode, pcm and end-to-end tests; the probe reads the encoded fixtures alone.
#[allow(dead_code)]
mod fixtures;

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::time::Duration;

use symphonia::core::io::MediaSource;
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

/// How long [`fixtures::TINY_OPUS`] states it plays, to the nanosecond.
///
/// Its last page states the stream playing up to 96 312: the [`fixtures::OPUS_FRAMES`] of tone with
/// the [`fixtures::OPUS_PRE_SKIP`] samples its encoder asks to have thrown away in front of them,
/// which a granule position counts from the start of the stream and which the demuxer states as the
/// frame count rather than subtracting what a decoder drops. Which is 96 312 /
/// [`fixtures::OPUS_RATE`] seconds — 2 seconds and 6 500 000 nanoseconds, and nothing left over.
const OPUS_STATED: Duration = Duration::new(2, 6_500_000);

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
fn an_opus_book_states_a_length_although_symphonia_decodes_no_opus() {
    let duration =
        probe_duration(Box::new(Cursor::new(fixtures::TINY_OPUS))).expect("a stated duration");

    // A conversion reads Ogg-Opus through libopus, because symphonia demuxes Ogg without having a
    // decoder for what an Opus stream holds. So the track a length is counted from cannot be the
    // track symphonia has a decoder for: here that is no track at all, and a book the converter
    // reads start to finish would state no length.
    assert_eq!(duration, OPUS_STATED);
}

#[test]
fn an_ogg_stream_that_is_not_opus_states_what_its_demuxer_states() {
    let duration =
        probe_duration(Box::new(Cursor::new(fixtures::VORBIS_OGG))).expect("a stated duration");

    // What sends an input down the Opus route is the Opus stream in it and not the Ogg around it:
    // this one is symphonia's to demux and to decode, and it states the 44 100 frames of tone it
    // was authored with at the 44 100 Hz it was authored at — one second, and nothing left over.
    assert_eq!(duration, Duration::from_secs(1));
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

#[test]
fn an_input_that_says_it_can_be_rewound_and_cannot_states_no_length() {
    // The first bytes of an input are read before anything else, to see which backend would read
    // it, and whatever reads them has to put the input back where it found it. One that says it is
    // seekable and then refuses is a broken input rather than an input of no length — which is what
    // a conversion makes of it too.
    let error = probe_duration(Box::new(SeeklessSource(Cursor::new(
        fixtures::TINY_M4B.to_vec(),
    ))))
    .expect_err("an input that cannot be rewound");

    assert!(matches!(error, ProbeError::Io(_)), "{error:?}");
    assert_eq!(error.to_string(), "reading the input failed");
}

/// An input that says it can be seeked and cannot: what a name that promises a file and hands over
/// a stream looks like from here.
struct SeeklessSource(Cursor<Vec<u8>>);

impl Read for SeeklessSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for SeeklessSource {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the input cannot be seeked after all",
        ))
    }
}

impl MediaSource for SeeklessSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}
