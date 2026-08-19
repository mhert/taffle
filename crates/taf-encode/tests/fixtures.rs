//! The audio the decode tests run on: the WAV files built here byte by byte, the Ogg streams built
//! here page by page, and the encoded ones committed next door.
//!
//! # The files on disk
//!
//! AAC and MPEG audio cannot be written out by hand, and the chapter marks and cover art this
//! crate reads out of their containers only exist once a muxer has put them there. So the encoded
//! fixtures are committed binaries, generated once with ffmpeg — `tests/fixtures/README.md` holds
//! the exact commands, the properties they were checked for, and everything the constants below
//! state about them.
//!
//! The MPEG and AAC ones carry the same tone: 10 seconds of a 440 Hz sine at 44 100 Hz, stereo,
//! peaking at [`ENCODED_PEAK`]. That peak is what a lossy codec has to bring back within a few
//! percent, and it is far enough from both silence and full scale that a sample conversion which
//! scales by the wrong power of two lands outside it. The Ogg ones carry the same sine at their
//! own lengths and peaks — [`OPUS_PEAK`], [`MONO_OPUS_PEAK`] — since Opus is always decoded at
//! 48 kHz and a mono stream is not upmixed before it is encoded.
//!
//! # The WAV files built here
//!
//! A WAV file is a RIFF container, and the smallest one that any decoder accepts is 44 bytes of
//! header — `RIFF`/`WAVE`, a 16-byte `fmt ` chunk, a `data` chunk header — followed by the samples
//! themselves, little-endian and interleaved. That is little enough to write out by hand, which is
//! what this module does: the tests depend on no encoder, no tool, and no file on disk, and the
//! bytes they decode are described in full right here.
//!
//! The tone is a 440 Hz sine, two seconds of it, at 44 100 Hz and 16 bits — a sample rate that is
//! *not* the 48 kHz a TAF ends up at, so nothing downstream can quietly assume the source already
//! arrives at the output rate. The two channels carry the same wave at different peaks
//! ([`LEFT_PEAK`] and [`RIGHT_PEAK`]): a decoder that swaps or collapses the channels, or that
//! reads planar samples as interleaved, gets the peaks wrong in a way a test can see.
//!
//! [`broken_adpcm_wav`] is the same idea turned around: a file that opens and then does not
//! decode.
//!
//! # The Ogg streams built here
//!
//! An Ogg page states a checksum over itself, so a stream cannot be cut out of a committed file
//! with an edit — which is what [`opus_pages`] is for: it lays packets out in pages with `taf`'s
//! own page builder, checksums and all, and every stream an error path needs is a different set of
//! packets handed to it. The audio packets are encoded by libopus itself ([`opus_frame`]), because
//! a stream that is only interesting once it is being decoded has to get that far first.

// Every cast below is on a compile-time constant that fits its target, or on a sine bounded by the
// peak it was scaled with.
#![allow(
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::f64::consts::TAU;

use opus::{Application, Channels, Encoder};
use taf::ogg::PageBuilder;

/// The fixture's sample rate, in Hz.
pub const SAMPLE_RATE: u32 = 44_100;

/// The fixture's channel count.
pub const CHANNELS: u16 = 2;

/// How long the fixture plays, in seconds.
pub const DURATION_SECS: u32 = 2;

/// The frequency of the tone, in Hz.
pub const TONE_HZ: f64 = 440.0;

/// What the left channel peaks at: 0.6 × [`i16::MAX`], rounded down.
pub const LEFT_PEAK: i16 = 19_660;

/// What the right channel peaks at: 0.3 × [`i16::MAX`], rounded down — half of [`LEFT_PEAK`], so
/// the two channels stay apart wherever they are compared.
pub const RIGHT_PEAK: i16 = 9_830;

/// Frames in the fixture, one sample per channel each.
pub const FRAMES: u32 = SAMPLE_RATE * DURATION_SECS;

/// Samples in the fixture: [`FRAMES`] × [`CHANNELS`].
pub const SAMPLES: usize = FRAMES as usize * CHANNELS as usize;

/// Bytes per sample: 16-bit PCM.
const BYTES_PER_SAMPLE: u32 = 2;

/// The tone in AAC, in an MP4 that states two chapters and carries [`COVER_PNG`] in a `covr` atom.
pub const TINY_M4B: &[u8] = include_bytes!("fixtures/tiny.m4b");

/// The tone in MPEG-1 Layer III, carrying [`COVER_PNG`] in the `APIC` frame of an ID3 tag,
/// and no chapters.
pub const TINY_MP3: &[u8] = include_bytes!("fixtures/tiny.mp3");

/// An MP4 whose first track is video and whose second one is a second of mono AAC.
pub const VIDEO_FIRST_MP4: &[u8] = include_bytes!("fixtures/video-first.mp4");

/// An MP4 of one video track and nothing else.
pub const NO_AUDIO_MP4: &[u8] = include_bytes!("fixtures/no-audio.mp4");

/// An MP4 carrying MPEG audio, which is a container and a codec that between them never state a
/// channel count.
pub const MP3_IN_MP4: &[u8] = include_bytes!("fixtures/mp3-in-mp4.mp4");

/// The cover art embedded in [`TINY_M4B`] and [`TINY_MP3`], byte for byte.
pub const COVER_PNG: &[u8] = include_bytes!("fixtures/cover.png");

/// How many frames of tone the encoded fixtures were authored with: 10 s at [`SAMPLE_RATE`], so
/// 441 000.
pub const ENCODED_FRAMES: u64 = 441_000;

/// What the tone peaks at before it is encoded, measured off the PCM ffmpeg authored it from.
pub const ENCODED_PEAK: i32 = 11_582;

/// Frames in one AAC packet, which is what a chapter mark in [`TINY_M4B`] can be off by: the
/// encoder's priming frames come out of the decoder as audio, and no timestamp accounts for them.
pub const AAC_FRAME: u64 = 1024;

/// The tone in Ogg-Opus: 2 seconds of stereo, which is 96 000 frames at [`OPUS_RATE`].
pub const TINY_OPUS: &[u8] = include_bytes!("fixtures/tiny.opus");

/// The same tone in Ogg-Opus with a single channel, and a second of it: what a stream that was
/// never stereo comes out of a stereo decoder as.
pub const MONO_OPUS: &[u8] = include_bytes!("fixtures/mono.opus");

/// An Ogg file that is not Opus: a second of Vorbis at [`SAMPLE_RATE`], which is the demuxer
/// behind `open_source`'s to read.
pub const VORBIS_OGG: &[u8] = include_bytes!("fixtures/vorbis.ogg");

/// The rate every Opus stream is decoded at, whatever was encoded into it.
pub const OPUS_RATE: u32 = 48_000;

/// Frames of tone in [`TINY_OPUS`]: 2 s at [`OPUS_RATE`].
pub const OPUS_FRAMES: u64 = 96_000;

/// Frames of tone in [`MONO_OPUS`]: 1 s at [`OPUS_RATE`].
pub const MONO_OPUS_FRAMES: u64 = 48_000;

/// Frames of tone in [`VORBIS_OGG`]: 1 s at [`SAMPLE_RATE`].
pub const VORBIS_FRAMES: u64 = 44_100;

/// Frames in one Opus packet as ffmpeg's libopus writes them, and as [`opus_frame`] encodes them:
/// 20 ms at [`OPUS_RATE`].
pub const OPUS_FRAME: u64 = 960;

/// What the Opus fixtures ask a decoder to throw away before the audio proper — libopus states
/// 312 samples, and both were encoded by it.
pub const OPUS_PRE_SKIP: u16 = 312;

/// What [`TINY_OPUS`]'s tone peaks at before it was encoded: ffmpeg's sine at 1/8 full scale,
/// upmixed to two channels with the standard centre-to-front coefficient of 1/√2 — 2 896.
pub const OPUS_PEAK: i32 = 2_896;

/// What [`MONO_OPUS`]'s tone peaks at before it was encoded: the same sine with no upmix to halve
/// it, so 1/8 of full scale — 4 095.
pub const MONO_OPUS_PEAK: i32 = 4_095;

/// Where [`TINY_M4B`]'s second chapter was authored: 5 seconds in.
///
/// The container states it in 100-nanosecond units — 50 000 000 of them — and at [`SAMPLE_RATE`]
/// that is 50 000 000 × 44 100 / 10 000 000 = 220 500 frames.
pub const M4B_SECOND_CHAPTER: u64 = 220_500;

/// What [`TINY_M4B`] calls its first chapter.
pub const M4B_FIRST_TITLE: &str = "Anfang";

/// What [`TINY_M4B`] calls its second chapter — an umlaut, so a title's length in bytes is not its
/// length in characters.
pub const M4B_SECOND_TITLE: &str = "Möhrchen macht Pause";

/// [`TINY_M4B`] with its AAC configuration rewritten to state the one thing this build's decoder
/// refuses outright: an object type of Main rather than the Low Complexity every m4b is written
/// in. The container still names a codec there is a decoder for — it just cannot be built from
/// what the container states about it.
///
/// # Panics
///
/// If the fixture stops stating an AAC configuration, which would mean it is no longer an m4b.
pub fn m4b_of_a_codec_that_cannot_be_set_up() -> Vec<u8> {
    // The configuration follows the descriptor tag that introduces it and the length ffmpeg writes
    // in its four-byte form.
    const INTRODUCED_BY: [u8; 4] = [0x05, 0x80, 0x80, 0x80];
    // Its first byte opens with five bits of object type; the three behind them begin the sample
    // rate index and are left alone.
    const MAIN: u8 = 1;

    let mut m4b = TINY_M4B.to_vec();
    let object_type = m4b
        .windows(INTRODUCED_BY.len())
        .position(|bytes| bytes == INTRODUCED_BY)
        .expect("the fixture states an AAC configuration")
        + INTRODUCED_BY.len()
        + 1;
    m4b[object_type] = (MAIN << 3) | (m4b[object_type] & 0b111);

    m4b
}

/// [`TINY_M4B`] with the two chapter marks it was authored with moved to `starts`, in the hundreds
/// of nanoseconds `chpl` counts in and in the order the atom lists them.
///
/// Which is how a container that lists its marks out of order looks, and how one that begins no
/// chapter at the start of the recording does — neither of which the fixture itself is, and both of
/// which a conversion has to hold up under. The titles are left where they are, so the mark a title
/// belongs to is the one it was authored on: [`M4B_FIRST_TITLE`] on `starts[0]` and
/// [`M4B_SECOND_TITLE`] on `starts[1]`.
///
/// # Panics
///
/// If the fixture stops stating the two chapter marks in a `chpl` atom, which would mean it is no
/// longer the m4b these tests are written against.
pub fn m4b_with_marks(starts: [u64; 2]) -> Vec<u8> {
    let mut m4b = TINY_M4B.to_vec();
    // The atom's contents follow the four bytes naming it, which follow the four stating its
    // length: a version byte and three of flags, four more from version 1 on, and the number of
    // marks in a single byte.
    let contents = m4b
        .windows(4)
        .position(|bytes| bytes == b"chpl")
        .expect("the fixture states a chapter atom")
        + 4;
    let versioned = usize::from(m4b[contents] > 0) * 4;
    let mut at = contents + 4 + versioned + 1;

    for start in starts {
        m4b[at..at + 8].copy_from_slice(&start.to_be_bytes());
        // A mark is its start, the length of its title in bytes, and the title itself.
        at += 8 + 1 + usize::from(m4b[at + 8]);
    }

    m4b
}

/// The whole fixture as the bytes of a WAV file.
pub fn sine_wav() -> Vec<u8> {
    let mut samples = Vec::with_capacity(SAMPLES);
    for frame in 0..FRAMES {
        let value = (TAU * TONE_HZ * f64::from(frame) / f64::from(SAMPLE_RATE)).sin();
        samples.push(scaled(value, LEFT_PEAK));
        samples.push(scaled(value, RIGHT_PEAK));
    }

    wav_of(SAMPLE_RATE, CHANNELS, &samples)
}

/// The bytes of a WAV file carrying `samples`, interleaved over `channels` at `rate`.
pub fn wav_of(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() as u32 * BYTES_PER_SAMPLE;
    let block_align = channels * BYTES_PER_SAMPLE as u16;

    let mut wav = Vec::with_capacity(44 + data_len as usize);

    // The RIFF header, whose length covers everything behind it.
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // The format chunk: 16 bytes of PCM description.
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // format tag 1: uncompressed PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&(BYTES_PER_SAMPLE as u16 * 8).to_le_bytes()); // bits per sample

    // The data chunk header, and behind it the samples.
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    wav
}

/// One sample of the sine: its value in −1.0..=1.0, at the peak the channel is written with.
fn scaled(value: f64, peak: i16) -> i16 {
    (value * f64::from(peak)).round() as i16
}

/// One Ogg page carrying `packet`, checksum and all.
///
/// The page that opens a stream is the one numbered zero, and it states so; `ends` states the end
/// of the stream, which is where the granule position stops being a running count and starts being
/// the sample the audio plays up to.
///
/// # Panics
///
/// If the packet is longer than the 4 096 bytes one page of a TAF holds, which no packet a test
/// here builds comes near.
pub fn ogg_page(serial: u32, sequence: u32, granule: u64, ends: bool, packet: &[u8]) -> Vec<u8> {
    ogg_page_of(serial, sequence, granule, ends, &[packet])
}

/// One Ogg page carrying every packet handed in, in the order they were handed in.
///
/// A page laces its packets one behind the other and states a lacing value per 255 bytes of each,
/// so what goes in front of the first packet is as long as everything behind it makes it.
///
/// # Panics
///
/// If the packets do not fit one page, whether by their length or by the 255 lacing values a page
/// counts them in.
pub fn ogg_page_of(
    serial: u32,
    sequence: u32,
    granule: u64,
    ends: bool,
    packets: &[&[u8]],
) -> Vec<u8> {
    let mut page = PageBuilder::new(serial, sequence);
    page.granule_position(granule);
    page.flags(sequence == 0, ends);
    for packet in packets {
        page.push_packet(packet).expect("the packets fit one page");
    }

    page.finish()
}

/// The pages of an Ogg-Opus stream of `serial`: `head`, whatever `tags` it states behind it, and
/// one page per audio packet.
///
/// One packet to a page, which keeps a lacing table out of what any test here is about. The
/// granule positions count what the stream plays up to — the pre-skip the head states plus one
/// [`OPUS_FRAME`] per audio packet, which is what [`opus_frame`] encodes — and the last audio page
/// states the end of the stream.
///
/// # Panics
///
/// If any packet is longer than one page holds — see [`ogg_page`].
pub fn opus_pages(serial: u32, head: &[u8], tags: Option<&[u8]>, audio: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut pages = vec![ogg_page(serial, 0, 0, false, head)];
    if let Some(tags) = tags {
        pages.push(ogg_page(serial, 1, 0, false, tags));
    }

    for (index, packet) in audio.iter().enumerate() {
        let plays_to = u64::from(OPUS_PRE_SKIP) + (index as u64 + 1) * OPUS_FRAME;
        let sequence = pages.len() as u32;
        pages.push(ogg_page(
            serial,
            sequence,
            plays_to,
            index + 1 == audio.len(),
            packet,
        ));
    }

    pages
}

/// The `OpusTags` packet a stream states behind its head, which nothing here reads.
///
/// # Panics
///
/// If the vendor string stops leaving the padding comment room, which is a constant here.
pub fn opus_tags() -> [u8; taf::ogg::OPUS_TAGS_LEN] {
    taf::ogg::opus_tags("taffle", &[]).expect("the vendor string leaves room for the padding")
}

/// One 20 ms Opus packet, encoded by libopus itself so that what decodes it has something real to
/// take apart.
///
/// The frame is silence: a stream built here is about the packets around this one, and silence
/// encodes to a handful of bytes.
///
/// # Panics
///
/// If libopus refuses the rate, the channel count or the frame length Opus itself defines.
pub fn opus_frame() -> Vec<u8> {
    /// Room for a packet that never comes close: 20 ms of silence encodes to single digits.
    const ROOM: usize = 4000;

    let frame = [0; OPUS_FRAME as usize * CHANNELS as usize];
    let mut encoder = Encoder::new(OPUS_RATE, Channels::Stereo, Application::Audio)
        .expect("libopus encodes at the rate it decodes at");

    encoder
        .encode_vec(&frame, ROOM)
        .expect("a frame of silence encodes")
}

/// A WAV whose header is sound and whose audio is not: one mono IMA ADPCM block that no decoder
/// can expand.
///
/// Every ADPCM block opens with a preamble of a predictor and a step index, and the step index
/// selects one of the 89 step sizes the codec is defined over — so a step index past those 89 is
/// data that cannot be decoded, while everything a demuxer looks at stays valid. That is a file
/// that gets as far as handing out a packet and then fails on it, which is the one thing a stream
/// of intact PCM can never do.
pub fn broken_adpcm_wav() -> Vec<u8> {
    // A block of 36 bytes is a 4-byte preamble and 32 bytes of nibble pairs: 64 encoded frames
    // plus the one the preamble's predictor carries.
    const BLOCK_ALIGN: u16 = 36;
    const FRAMES_PER_BLOCK: u16 = 65;
    // Past the 88 the codec's step table ends at.
    const INVALID_STEP_INDEX: u8 = 0xff;

    let data_len = u32::from(BLOCK_ALIGN);
    let fmt_len = 20_u32;

    let mut wav = Vec::with_capacity(12 + 8 + fmt_len as usize + 8 + data_len as usize);

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(4 + (8 + fmt_len) + (8 + data_len)).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // The format chunk: 16 bytes of description and the 4-byte extension IMA ADPCM requires.
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&fmt_len.to_le_bytes());
    wav.extend_from_slice(&0x0011_u16.to_le_bytes()); // format tag 0x11: IMA ADPCM
    wav.extend_from_slice(&1_u16.to_le_bytes()); // one channel
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(
        &(SAMPLE_RATE * u32::from(BLOCK_ALIGN) / u32::from(FRAMES_PER_BLOCK)).to_le_bytes(),
    ); // bytes per second
    wav.extend_from_slice(&BLOCK_ALIGN.to_le_bytes());
    wav.extend_from_slice(&4_u16.to_le_bytes()); // four bits per encoded sample
    wav.extend_from_slice(&2_u16.to_le_bytes()); // two bytes of extension follow
    wav.extend_from_slice(&FRAMES_PER_BLOCK.to_le_bytes());

    // The data chunk: one block, whose preamble names a step size the codec does not have.
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&0_u16.to_le_bytes()); // predictor
    wav.push(INVALID_STEP_INDEX);
    wav.push(0); // reserved
    wav.resize(wav.len() + usize::from(BLOCK_ALIGN) - 4, 0);

    wav
}
