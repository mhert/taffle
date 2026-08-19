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

/// One of the committed fixtures, opened.
fn source_of(fixture: &'static [u8]) -> Box<dyn AudioSource> {
    open_source(Box::new(Cursor::new(fixture))).unwrap()
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

/// How many frames a run of blocks holds, at the channel count they interleave.
fn frames_in(blocks: &[Vec<i16>], channels: u16) -> u64 {
    let samples: usize = blocks.iter().map(Vec::len).sum();
    u64::try_from(samples / usize::from(channels)).unwrap()
}

/// The loudest sample anywhere in a run of blocks.
fn peak_in(blocks: &[Vec<i16>]) -> i32 {
    blocks
        .iter()
        .flatten()
        .map(|sample| i32::from(*sample).abs())
        .max()
        .unwrap()
}

/// A lossy codec is allowed to miss the tone's peak, and both fixtures were measured missing it by
/// a few percent. What none of them may do is miss it by the factor a sample conversion that
/// scales twice, or not at all, would: a seventh either way is wide enough for the codecs and far
/// too narrow for that.
fn assert_peak_near_the_authored_one(blocks: &[Vec<i16>]) {
    let peak = peak_in(blocks);
    let authored = fixtures::ENCODED_PEAK;
    let slack = authored / 7;

    assert!(
        (authored - slack..=authored + slack).contains(&peak),
        "the tone peaks at {peak}, not within a seventh of the authored {authored}"
    );
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
fn an_m4b_states_the_chapter_marks_it_was_authored_with() {
    let mut source = source_of(fixtures::TINY_M4B);

    let metadata = source.metadata();

    let starts: Vec<u64> = metadata.chapters.iter().map(|ch| ch.start_sample).collect();
    let titles: Vec<Option<&str>> = metadata
        .chapters
        .iter()
        .map(|ch| ch.title.as_deref())
        .collect();
    assert_eq!(
        titles,
        [
            Some(fixtures::M4B_FIRST_TITLE),
            Some(fixtures::M4B_SECOND_TITLE)
        ]
    );
    assert_eq!(starts.first(), Some(&0));
    // The mark is the container's own timestamp, which counts from the first frame the file was
    // authored with rather than from the priming frames the AAC decoder hands out ahead of it.
    let second = starts.get(1).copied().expect("a second chapter");
    let authored = fixtures::M4B_SECOND_CHAPTER;
    assert!(
        (authored - fixtures::AAC_FRAME..=authored + fixtures::AAC_FRAME).contains(&second),
        "the second chapter starts at frame {second}, not within a packet of {authored}"
    );
}

#[test]
fn an_m4b_carries_its_cover_through_byte_for_byte() {
    let mut source = source_of(fixtures::TINY_M4B);

    let cover = source.metadata().cover.expect("the m4b carries a cover");

    assert_eq!(cover.mime, "image/png");
    assert_eq!(cover.bytes, fixtures::COVER_PNG);
}

#[test]
fn an_m4b_decodes_the_tone_it_was_authored_with() {
    let mut source = source_of(fixtures::TINY_M4B);

    let blocks = blocks_of(source.as_mut());

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::SAMPLE_RATE,
            channels: fixtures::CHANNELS,
        }
    );
    // AAC decodes in packets of whole frames, and the stream is the authored tone with the
    // encoder's priming and padding around it — under one packet of each.
    let frames = frames_in(&blocks, fixtures::CHANNELS);
    assert!(
        (fixtures::ENCODED_FRAMES..fixtures::ENCODED_FRAMES + 2 * fixtures::AAC_FRAME)
            .contains(&frames),
        "decoded {frames} frames, not the authored {} and its priming",
        fixtures::ENCODED_FRAMES
    );
    assert_peak_near_the_authored_one(&blocks);
}

#[test]
fn an_mp3_decodes_the_tone_without_the_padding_its_encoder_added() {
    let mut source = source_of(fixtures::TINY_MP3);

    let blocks = blocks_of(source.as_mut());

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::SAMPLE_RATE,
            channels: fixtures::CHANNELS,
        }
    );
    // An MPEG encoder starts a stream with frames of its own and pads the last one out, and says
    // in the header how much of each it added: exactly the authored tone is left when both go.
    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        fixtures::ENCODED_FRAMES
    );
    assert_peak_near_the_authored_one(&blocks);
}

#[test]
fn an_mp3_carries_its_cover_through_byte_for_byte() {
    let mut source = source_of(fixtures::TINY_MP3);

    let metadata = source.metadata();

    let cover = metadata.cover.expect("the mp3 carries a cover");
    assert_eq!(cover.mime, "image/png");
    assert_eq!(cover.bytes, fixtures::COVER_PNG);
    // Chapter marks live in the MP4 atom this crate reads; an MP3 states none it can find.
    assert!(metadata.chapters.is_empty());
}

#[test]
fn a_track_of_video_ahead_of_the_audio_is_not_the_one_decoded() {
    let mut source = source_of(fixtures::VIDEO_FIRST_MP4);

    let blocks = blocks_of(source.as_mut());

    // The audio in this container is a second of mono: a source that took the container's first
    // track would have found no audio in it at all.
    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::SAMPLE_RATE,
            channels: 1,
        }
    );
    let frames = frames_in(&blocks, 1);
    assert!(
        frames >= u64::from(fixtures::SAMPLE_RATE),
        "decoded {frames} frames of the second of audio the fixture holds"
    );
}

#[test]
fn audio_whose_shape_nothing_states_is_not_a_format_this_reads() {
    // An MP4 states its audio's sample rate and leaves the channel count to the codec, and MPEG
    // audio keeps it in the frames themselves — so between the two, nothing says how many channels
    // there are until a frame has been decoded, and a source states its shape before that.
    let opened = open_source(Box::new(Cursor::new(fixtures::MP3_IN_MP4)));

    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "MPEG audio in an MP4 opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn a_codec_configuration_this_build_cannot_read_is_not_a_format_it_reads() {
    let opened = open_source(Box::new(Cursor::new(
        fixtures::m4b_of_a_codec_that_cannot_be_set_up(),
    )));

    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "an m4b whose codec cannot be set up opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn an_input_that_says_it_can_be_rewound_and_cannot_reports_the_read_failure() {
    // Chapter marks are read off the input before anything else, and what reads them has to put
    // it back where it found it. One that says it is seekable and then refuses is a broken input,
    // not an input of no chapters.
    let opened = open_source(Box::new(SeeklessSource(Cursor::new(
        fixtures::TINY_M4B.to_vec(),
    ))));

    assert!(
        matches!(opened, Err(DecodeError::Io(_))),
        "an input that cannot be rewound opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn a_container_of_no_audio_at_all_has_nothing_to_decode() {
    let opened = open_source(Box::new(Cursor::new(fixtures::NO_AUDIO_MP4)));

    assert!(
        matches!(opened, Err(DecodeError::NoAudioTrack)),
        "an MP4 of one video track opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
#[ignore = "reads an audiobook that only exists on the machine this crate is written on"]
fn a_real_audiobook_is_a_codec_this_build_cannot_decode() {
    let path = "/home/mhert/OpenAudible/books/\
                Grimm und Möhrchen machen Pause von zu Hause (Teil 3).m4b";
    let book = std::fs::File::open(path).expect("the book is on this machine");

    let opened = open_source(Box::new(book));

    // Every audiobook on this machine states an AAC configuration of two bytes whose
    // core-coder-dependency flag is set, which leaves no room for the fourteen bits of delay that
    // flag calls for. Other decoders read past it and decode the audio; symphonia's stops there,
    // so none of these books can be converted yet. The chapter marks and the cover art *are* read
    // out of them — that happens before any decoder is built, and the module that does it asserts
    // this book's sixteen chapters. This is the rest of the way, and when it can be walked the
    // assertion below is what has to change.
    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "the book opened as {:?}",
        opened.map(|_| "a source")
    );
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
