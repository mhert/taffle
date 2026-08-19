//! What [`open_source`] makes of an input: the stream it reports, the samples it hands out, and
//! what it says about bytes it cannot make sense of.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod fixtures;

use std::fs;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;

use symphonia::core::io::{MediaSource, ReadOnlySource};
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

/// A lossy codec is allowed to miss the tone's peak, and every fixture was measured missing it by
/// a few percent. What none of them may do is miss it by the factor a sample conversion that
/// scales twice, or not at all, would: a seventh either way is wide enough for the codecs and far
/// too narrow for that.
fn assert_peak_near(peak: i32, authored: i32) {
    let slack = authored / 7;

    assert!(
        (authored - slack..=authored + slack).contains(&peak),
        "the tone peaks at {peak}, not within a seventh of the authored {authored}"
    );
}

/// The failure a source runs into before it runs out of blocks.
///
/// The walk is bounded the same way [`blocks_of`] is, and both ways of not failing — a clean end
/// and no end at all — are what this is asserting against.
fn failure_of(source: &mut dyn AudioSource) -> DecodeError {
    for _ in 0..fixtures::SAMPLES {
        match source.next_block() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the broken input ended as if it were whole"),
            Err(err) => return err,
        }
    }
    panic!("the broken input kept handing out blocks");
}

/// An Ogg-Opus file of the packets handed in, one page each.
fn opus_file(head: &[u8], tags: Option<&[u8]>, audio: &[&[u8]]) -> Vec<u8> {
    fixtures::opus_pages(1, head, tags, audio).concat()
}

/// `bytes` written to a file of `name` in the temporary directory, and where it went.
///
/// The name carries the process the test runs in, so that two runs at once do not write over each
/// other's inputs.
fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("taf-encode-{}-{name}", std::process::id()));
    fs::write(&path, bytes).expect("the temporary directory takes a file");

    path
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
    assert_peak_near(peak_in(&blocks), fixtures::ENCODED_PEAK);
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
    assert_peak_near(peak_in(&blocks), fixtures::ENCODED_PEAK);
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
fn an_opus_stream_is_decoded_at_the_48_khz_opus_is_defined_at() {
    let source = source_of(fixtures::TINY_OPUS);

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::OPUS_RATE,
            channels: fixtures::CHANNELS,
        }
    );
}

#[test]
fn an_opus_stream_decodes_the_tone_without_the_samples_its_encoder_added() {
    let mut source = source_of(fixtures::TINY_OPUS);

    let blocks = blocks_of(source.as_mut());

    // The fixture states a pre-skip of 312 samples and a last granule position of 96 312, so the
    // 96 000 frames it was authored with are exactly what is left when both ends are trimmed to
    // what the stream says about itself.
    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        fixtures::OPUS_FRAMES
    );
    assert_peak_near(peak_in(&blocks), fixtures::OPUS_PEAK);
}

#[test]
fn an_opus_stream_states_no_chapters_and_no_cover() {
    let mut source = source_of(fixtures::TINY_OPUS);

    let metadata = source.metadata();

    assert!(metadata.chapters.is_empty());
    assert!(metadata.cover.is_none());
}

#[test]
fn the_bytes_decide_which_backend_reads_an_input_and_not_the_name_it_came_under() {
    // `open_source` is handed a reader and never a name, so the only name an input can arrive
    // under is the one on the file it was opened from. This one says MPEG audio and holds Opus.
    let lying = temp_file("says-mp3-holds-opus.mp3", fixtures::TINY_OPUS);
    let mut source = open_source(Box::new(fs::File::open(&lying).unwrap())).unwrap();

    let blocks = blocks_of(source.as_mut());

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::OPUS_RATE,
            channels: fixtures::CHANNELS,
        }
    );
    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        fixtures::OPUS_FRAMES
    );
    fs::remove_file(&lying).unwrap();
}

#[test]
fn a_mono_opus_stream_arrives_as_the_stereo_every_opus_source_hands_out() {
    let mut source = source_of(fixtures::MONO_OPUS);

    let blocks = blocks_of(source.as_mut());

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::OPUS_RATE,
            channels: fixtures::CHANNELS,
        }
    );
    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        fixtures::MONO_OPUS_FRAMES
    );
    // libopus hands a stereo decoder the one channel a mono stream carries, on both sides — so
    // the tone comes out at the peak it was authored with rather than at half of it, and the two
    // sides are the same sample for sample.
    let left = blocks.iter().map(|block| peak_of(block, 0)).max().unwrap();
    let right = blocks.iter().map(|block| peak_of(block, 1)).max().unwrap();
    assert_eq!(
        left, right,
        "one channel came out of the two sides differently"
    );
    assert_peak_near(left, fixtures::MONO_OPUS_PEAK);
}

#[test]
fn an_ogg_stream_that_is_not_opus_is_read_by_the_demuxer_behind_the_sniff() {
    let mut source = source_of(fixtures::VORBIS_OGG);

    let blocks = blocks_of(source.as_mut());

    // Vorbis in Ogg is symphonia's to demux and decode, and it states the rate the file was
    // authored at — not the 48 kHz every Opus stream is decoded at.
    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::SAMPLE_RATE,
            channels: fixtures::CHANNELS,
        }
    );
    // Vorbis decodes in blocks of up to 2048 frames and symphonia hands out the last one whole,
    // so the second the file was authored with comes back with under a block on top of it.
    let frames = frames_in(&blocks, fixtures::CHANNELS);
    assert!(
        (fixtures::VORBIS_FRAMES..fixtures::VORBIS_FRAMES + 2048).contains(&frames),
        "decoded {frames} frames, not the authored {} and its last block",
        fixtures::VORBIS_FRAMES
    );
}

#[test]
fn an_opus_stream_whose_channels_take_more_than_one_decoder_is_not_a_format_this_reads() {
    // Channel mapping family 1 is surround: several Opus streams in one, which takes a decoder
    // per stream and the mapping table behind the head to place them.
    let mut head = taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP);
    head[18] = 1;

    let opened = open_source(Box::new(Cursor::new(opus_file(
        &head,
        Some(&fixtures::opus_tags()),
        &[&fixtures::opus_frame()],
    ))));

    assert!(
        matches!(opened, Err(DecodeError::UnsupportedFormat)),
        "a surround stream opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn an_opus_stream_that_does_not_state_its_comment_header_never_was_one() {
    // RFC 7845 puts the comment header in the second packet of the stream and nowhere else, so a
    // stream that ends in front of it, one that breaks off inside it, and one that states
    // something else there are all the same kind of broken: not the format the first packet
    // claimed, rather than a file that could not be read.
    let head = taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP);
    let whole = opus_file(&head, Some(&fixtures::opus_tags()), &[]);

    let ended = open_source(Box::new(Cursor::new(opus_file(&head, None, &[]))));
    let broken = open_source(Box::new(Cursor::new(
        whole.get(..whole.len() - 200).map(<[u8]>::to_vec).unwrap(),
    )));
    let other = open_source(Box::new(Cursor::new(opus_file(
        &head,
        Some(b"not the comment header"),
        &[&fixtures::opus_frame()],
    ))));

    assert!(
        matches!(ended, Err(DecodeError::UnsupportedFormat)),
        "a stream of nothing but a head opened as {:?}",
        ended.map(|_| "a source")
    );
    assert!(
        matches!(broken, Err(DecodeError::UnsupportedFormat)),
        "a stream breaking off inside its comment header opened as {:?}",
        broken.map(|_| "a source")
    );
    assert!(
        matches!(other, Err(DecodeError::UnsupportedFormat)),
        "a stream of no comment header opened as {:?}",
        other.map(|_| "a source")
    );
}

#[test]
fn a_head_behind_the_longest_lacing_table_a_page_can_state_is_still_found() {
    /// How many lacing values a page states at most, which is what one byte counts to.
    const VALUES: usize = 255;
    /// What the two headers take of them: one for the head, two for the comment header, whose
    /// 436 bytes need a value for their first full 255-byte segment and one to end them.
    const HEADERS: usize = 3;
    /// Where the first packet of such a page begins: the page's header and its whole table.
    const MAGIC_AT: usize = 27 + VALUES;

    let head = taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP);
    let tags = fixtures::opus_tags();
    // Packets of no bytes, one lacing value each, padding the table out to everything a page can
    // state — which is the furthest into a page the magic the sniff looks for can be pushed.
    let mut packets: Vec<&[u8]> = vec![&head, &tags];
    packets.resize(packets.len() + VALUES - HEADERS, &[]);
    let page = fixtures::ogg_page_of(1, 0, 0, false, &packets);
    assert_eq!(page.get(MAGIC_AT..MAGIC_AT + 8), Some(&b"OpusHead"[..]));

    let source = open_source(Box::new(Cursor::new(page))).expect("the stream opens");

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::OPUS_RATE,
            channels: fixtures::CHANNELS,
        }
    );
}

#[test]
fn opus_audio_that_does_not_decode_is_not_a_clean_end() {
    // A packet that says a frame count follows it and then ends: nothing libopus can take apart.
    let file = opus_file(
        &taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP),
        Some(&fixtures::opus_tags()),
        &[&[0xff]],
    );
    let mut source = open_source(Box::new(Cursor::new(file))).expect("the stream opens");

    let failure = failure_of(source.as_mut());

    assert!(
        matches!(failure, DecodeError::Decode(_)),
        "reported as {failure:?}"
    );
}

#[test]
fn an_opus_packet_of_no_bytes_at_all_is_broken_data_and_not_a_lost_packet() {
    // libopus reads a packet of no bytes as one that went missing and invents audio to cover the
    // gap. A file that states one is broken rather than lossy, and gets told so.
    let file = opus_file(
        &taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP),
        Some(&fixtures::opus_tags()),
        &[&[]],
    );
    let mut source = open_source(Box::new(Cursor::new(file))).expect("the stream opens");

    let failure = failure_of(source.as_mut());

    assert!(
        matches!(failure, DecodeError::Decode(_)),
        "reported as {failure:?}"
    );
}

#[test]
fn a_page_whose_checksum_does_not_cover_what_it_carries_is_not_decoded() {
    // A byte in the middle of the first audio page's body, which the page's checksum states a
    // sum over: what a file that rotted on disk looks like from here.
    let mut corrupt = fixtures::TINY_OPUS.to_vec();
    corrupt[5_000] ^= 0xff;
    let mut source = open_source(Box::new(Cursor::new(corrupt))).expect("the stream opens");

    let failure = failure_of(source.as_mut());

    assert!(
        matches!(failure, DecodeError::Decode(_)),
        "reported as {failure:?}"
    );
}

#[test]
fn an_opus_stream_that_breaks_off_mid_page_is_not_a_clean_end() {
    let truncated = fixtures::TINY_OPUS
        .get(..fixtures::TINY_OPUS.len() - 512)
        .map(<[u8]>::to_vec)
        .unwrap();
    let mut source = open_source(Box::new(Cursor::new(truncated))).expect("the stream opens");

    let failure = failure_of(source.as_mut());

    assert!(
        matches!(failure, DecodeError::Io(_)),
        "reported as {failure:?}"
    );
}

#[test]
fn the_packets_of_another_stream_in_the_same_file_are_not_decoded_as_opus() {
    let frame = fixtures::opus_frame();
    let mut pages = fixtures::opus_pages(
        1,
        &taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP),
        Some(&fixtures::opus_tags()),
        &[&frame, &frame],
    );
    // An Ogg file may carry several logical streams at once: the pages opening them all stand in
    // front of the audio, and from there the streams take turns. This one is not Opus, and no
    // Opus decoder could make anything of either of its packets.
    pages.insert(1, fixtures::ogg_page(2, 0, 0, false, b"something else"));
    pages.insert(4, fixtures::ogg_page(2, 1, 0, false, b"and more of it"));
    let mut source =
        open_source(Box::new(Cursor::new(pages.concat()))).expect("the opus stream opens");

    let blocks = blocks_of(source.as_mut());

    // Two frames of audio, and the pre-skip the head states trimmed off the front of them.
    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        2 * fixtures::OPUS_FRAME - u64::from(fixtures::OPUS_PRE_SKIP)
    );
}

#[test]
fn only_the_page_a_stream_ends_on_says_where_its_audio_stops() {
    // RFC 7845 gives the last page's granule position the meaning "the audio stops here", and
    // every page in front of it states a timestamp — one a stream is free to carry an offset in.
    // A timestamp that lags what its packets decoded to is not a stream stating less audio.
    let frame = fixtures::opus_frame();
    let plays_to = u64::from(fixtures::OPUS_PRE_SKIP) + 2 * fixtures::OPUS_FRAME;
    let file = [
        fixtures::ogg_page(
            1,
            0,
            0,
            false,
            &taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP),
        ),
        fixtures::ogg_page(1, 1, 0, false, &fixtures::opus_tags()),
        fixtures::ogg_page(1, 2, 100, false, &frame),
        fixtures::ogg_page(1, 3, plays_to, true, &frame),
    ]
    .concat();
    let mut source = open_source(Box::new(Cursor::new(file))).expect("the stream opens");

    let blocks = blocks_of(source.as_mut());

    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        2 * fixtures::OPUS_FRAME - u64::from(fixtures::OPUS_PRE_SKIP)
    );
}

#[test]
fn a_pre_skip_longer_than_a_packet_takes_whole_packets_with_it() {
    // libopus asks for 312 samples; the format allows any number, and an encoder asking for more
    // than one packet holds leaves that packet none of the recording.
    const PRE_SKIP: u16 = 2_000;

    let frame = fixtures::opus_frame();
    let plays_to = u64::from(PRE_SKIP) + 3 * fixtures::OPUS_FRAME;
    let file = [
        fixtures::ogg_page(1, 0, 0, false, &taf::ogg::opus_head(PRE_SKIP)),
        fixtures::ogg_page(1, 1, 0, false, &fixtures::opus_tags()),
        fixtures::ogg_page(
            1,
            2,
            u64::from(PRE_SKIP) + fixtures::OPUS_FRAME,
            false,
            &frame,
        ),
        fixtures::ogg_page(
            1,
            3,
            u64::from(PRE_SKIP) + 2 * fixtures::OPUS_FRAME,
            false,
            &frame,
        ),
        fixtures::ogg_page(1, 4, plays_to, true, &frame),
    ]
    .concat();
    let mut source = open_source(Box::new(Cursor::new(file))).expect("the stream opens");

    let blocks = blocks_of(source.as_mut());

    assert_eq!(
        frames_in(&blocks, fixtures::CHANNELS),
        3 * fixtures::OPUS_FRAME - u64::from(PRE_SKIP)
    );
}

#[test]
fn an_opus_stream_whose_header_pages_do_not_hold_together_never_was_one() {
    // A byte behind the magic of the head, and one inside the comment header behind it: both sit
    // in a page whose checksum states a sum over them, and both leave a file that is sniffed as
    // Opus and then found not to hold a stream.
    for byte in [40, 100] {
        let mut corrupt = fixtures::TINY_OPUS.to_vec();
        corrupt[byte] ^= 0xff;

        let opened = open_source(Box::new(Cursor::new(corrupt)));

        assert!(
            matches!(opened, Err(DecodeError::UnsupportedFormat)),
            "a stream broken at byte {byte} opened as {:?}",
            opened.map(|_| "a source")
        );
    }
}

#[test]
fn an_input_that_breaks_off_while_it_is_being_sniffed_is_no_opus_stream() {
    // The sniff reads before anything else does, and 20 bytes is not enough of a page to reach
    // the magic that decides. An input that breaks off inside them states no Opus and is handed
    // on unread, so what comes back is the read failure the backend behind the sniff runs into —
    // not a broken Opus stream.
    let opened = open_source(Box::new(FailingSource::seekable(
        fixtures::TINY_OPUS.to_vec(),
        20,
    )));

    assert!(
        matches!(opened, Err(DecodeError::Io(_))),
        "an input that broke off mid-sniff opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn an_input_that_stops_being_readable_in_front_of_the_audio_reports_the_read_failure() {
    // Enough of the stream to sniff it as Opus and to read its head, and not enough to finish the
    // comment header behind it — which is a page or more of its own in any file carrying a cover.
    let file = opus_file(
        &taf::ogg::opus_head(fixtures::OPUS_PRE_SKIP),
        Some(&fixtures::opus_tags()),
        &[],
    );

    let opened = open_source(Box::new(FailingSource::seekable(file, 400)));

    assert!(
        matches!(opened, Err(DecodeError::Io(_))),
        "an input that broke off opened as {:?}",
        opened.map(|_| "a source")
    );
}

#[test]
fn an_opus_stream_that_cannot_be_seeked_is_not_one_this_reads() {
    // The sniff rewinds the input, and the reader an Opus stream is taken apart with seeks in it
    // as well, so a stream that arrives through something unseekable never reaches either. What
    // does reach it is symphonia, which demuxes the Ogg and finds no track it has a decoder for.
    let opened = open_source(Box::new(ReadOnlySource::new(Cursor::new(
        fixtures::TINY_OPUS,
    ))));

    assert!(
        matches!(opened, Err(DecodeError::NoAudioTrack)),
        "an unseekable Opus stream opened as {:?}",
        opened.map(|_| "a source")
    );
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
fn a_real_audiobook_decodes_whole_with_everything_it_states_about_itself() {
    // 64 minutes and 12 seconds of it, at 44 100 Hz.
    const SECONDS: u64 = 3_852;

    let path = "/home/mhert/OpenAudible/books/\
                Grimm und Möhrchen machen Pause von zu Hause (Teil 3).m4b";
    let book = std::fs::File::open(path).expect("the book is on this machine");

    let mut source = open_source(Box::new(book)).expect("the book opens");

    assert_eq!(
        source.spec(),
        SourceSpec {
            sample_rate: fixtures::SAMPLE_RATE,
            channels: fixtures::CHANNELS,
        }
    );
    let metadata = source.metadata();
    let titles: Vec<Option<&str>> = metadata
        .chapters
        .iter()
        .map(|chapter| chapter.title.as_deref())
        .collect();
    assert_eq!(titles.len(), 16);
    assert_eq!(titles.first(), Some(&Some("Kapitel 1")));
    assert_eq!(titles.last(), Some(&Some("Kapitel 16")));
    // The book's second chapter is authored at 195.395 s, which at 44 100 Hz is frame 8 616 919.
    assert_eq!(
        metadata.chapters.get(1).map(|chapter| chapter.start_sample),
        Some(8_616_919)
    );
    let cover = metadata.cover.expect("the book carries a cover");
    assert_eq!(cover.mime, "image/jpeg");
    assert_eq!(cover.bytes.len(), 70_200);

    // The whole book, decoded — every packet of it, which is the part that took repairing the
    // core coder dependency its configuration states and has no room to describe.
    let mut frames = 0;
    let mut peak = 0;
    while let Some(block) = source.next_block().expect("the book decodes") {
        let block = std::slice::from_ref(&block);
        frames += frames_in(block, fixtures::CHANNELS);
        peak = peak.max(peak_in(block));
    }

    let seconds = frames / u64::from(fixtures::SAMPLE_RATE);
    assert!(
        (SECONDS..=SECONDS + 1).contains(&seconds),
        "decoded {seconds} seconds of a book of {SECONDS}"
    );
    // Speech, not the silence a decoder that ran but understood nothing would hand out.
    assert!(peak > i32::from(i16::MAX) / 4, "the book decoded to {peak}");
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

    let failure = failure_of(source.as_mut());

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
    seekable: bool,
}

impl FailingSource {
    fn new(bytes: Vec<u8>, readable: u64) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            readable,
            seekable: false,
        }
    }

    /// The same input, saying it can be seeked — which is what the Opus route asks of one before
    /// it reads anything at all.
    fn seekable(bytes: Vec<u8>, readable: u64) -> Self {
        Self {
            seekable: true,
            ..Self::new(bytes, readable)
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
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}
