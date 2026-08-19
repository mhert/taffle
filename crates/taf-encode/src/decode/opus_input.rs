//! [`AudioSource`] over libopus: the Opus an Ogg file carries, decoded straight into the 48 kHz
//! stereo everything behind this module works in.
//!
//! # Why this is not symphonia's job
//!
//! symphonia demuxes Ogg and decodes Vorbis, and has no Opus decoder at all. An Ogg-Opus file
//! handed to its probe is therefore claimed by the Ogg demuxer and only then found to hold a codec
//! nothing can be built for — with the input read past by the time that is known. So the sniff
//! this module states runs in *front* of the probe, and decides by what the bytes are rather than
//! by what any name says they are.
//!
//! # What a stream states, and what libopus does with it
//!
//! Opus is defined at 48 kHz, and the decoder here is asked for the two channels a TAF carries: a
//! stereo stream comes out as it was authored and a mono one comes out on both sides, whatever the
//! head states about its channels — which is why nothing here reads that. What the head *is* read
//! for is its pre-skip, the samples an encoder needs before it is up to speed, and the last page's
//! granule position says where the recording stops at the other end. Both are trimmed away, so
//! what a source hands out is the recording and not what an encoder wrapped it in — the same thing
//! `enable_gapless` arranges on the symphonia route, and for the same reason: an audiobook is
//! converted from files that were cut at chapter boundaries, and every seam would otherwise carry
//! the padding of the file in front of it.
//!
//! # What this does not read
//!
//! An Opus stream states everything it says about itself in the comment header, which may carry
//! cover art as a `METADATA_BLOCK_PICTURE` comment — a FLAC picture block in base64. Nothing here
//! reads that, and a source over Opus states no metadata at all.
//!
//! An Ogg file may also *chain* streams: a second logical stream behind the end of the first, as
//! concatenating two files leaves them. Only the stream the file opens with is decoded, since that
//! is the one whose head was read. Streams that run *alongside* each other in one file are
//! something else and are handled: the packets of every stream but this one are passed over.

use std::io::{self, ErrorKind, SeekFrom};
use std::ops::Range;

use ogg::{OggReadError, Packet, PacketReader};
use opus::{Channels, Decoder};
use symphonia::core::io::MediaSource;

use super::{AudioSource, DecodeError, SourceMetadata, SourceSpec};

/// The rate Opus is defined at, and so the only one anything is decoded at.
const RATE: u32 = 48_000;

/// How many channels every block comes out in, whatever the stream was authored in.
const CHANNELS: u16 = 2;

/// The most samples per channel one Opus packet decodes to: 120 ms at [`RATE`].
const MAX_FRAME: usize = 5_760;

/// What every Ogg page starts with.
const CAPTURE_PATTERN: &[u8] = b"OggS";

/// What the packet opening an Opus stream starts with.
const HEAD_MAGIC: &[u8] = b"OpusHead";

/// What the packet behind it starts with.
const TAGS_MAGIC: &[u8] = b"OpusTags";

/// The bytes RFC 3533 puts in front of a page's lacing table.
const PAGE_HEADER_LEN: usize = 27;

/// How far into that header the byte counting the lacing values sits.
const SEGMENT_COUNT_AT: usize = 26;

/// The most lacing values a page states, which is what that byte can count to.
const MAX_SEGMENTS: usize = 255;

/// How many bytes of an input decide whether it is Ogg-Opus: a page header, the longest lacing
/// table one can state, and the magic of the packet behind it.
const SNIFF_LEN: usize = PAGE_HEADER_LEN + MAX_SEGMENTS + HEAD_MAGIC.len();

/// The packets of the physical stream an input holds.
type Packets = PacketReader<Box<dyn MediaSource>>;

/// Whether the input opens an Ogg-Opus stream, with the input left where it was found.
///
/// An input that cannot seek is not sniffed and states nothing: reading its first bytes would eat
/// what the backend behind this needs, and there would be no giving them back. Nothing is lost by
/// that — the reader an Opus stream is taken apart with seeks as well, so an input that cannot is
/// one this module could not decode either way.
///
/// # Errors
///
/// [`io::Error`] when the input cannot be rewound after being read. Bytes that cannot be read at
/// all are an input that states no Opus — whatever is wrong with it is the next backend's to
/// report, from the same place this started reading.
pub(super) fn sniff(source: &mut dyn MediaSource) -> io::Result<bool> {
    if !source.is_seekable() {
        return Ok(false);
    }

    let mut prefix = [0; SNIFF_LEN];
    let read = fill(source, &mut prefix).unwrap_or(0);
    source.seek(SeekFrom::Start(0))?;

    Ok(opens_an_opus_stream(prefix.get(..read).unwrap_or_default()))
}

/// Opens the Ogg-Opus stream the input holds.
///
/// # Errors
///
/// [`DecodeError::UnsupportedFormat`] when the stream is not one this build can decode: a file
/// whose pages do not hold the two headers RFC 7845 opens a stream with, one stating a version
/// this does not know, or one whose channels are mapped across several Opus streams — which takes
/// a decoder per stream and is not what an audiobook is authored as.
/// [`DecodeError::Io`] when the input itself cannot be read, and [`DecodeError::Decode`] when
/// libopus refuses to set up a decoder at all.
pub(super) fn open(reader: Box<dyn MediaSource>) -> Result<Box<dyn AudioSource>, DecodeError> {
    let mut packets = PacketReader::new(reader);

    let opening = next_packet(&mut packets)?;
    let stream = opening.stream_serial();
    let head = OpusHead::of(&opening.data).ok_or(DecodeError::UnsupportedFormat)?;

    // RFC 7845 puts the comment header in the second packet of the stream and nowhere else.
    // Nothing here reads it, but a stream that states none there is not what its first packet
    // claimed, and the packets behind it are not audio either.
    if !next_packet_of(&mut packets, stream)?
        .data
        .starts_with(TAGS_MAGIC)
    {
        return Err(DecodeError::UnsupportedFormat);
    }

    // Nothing but memory can stop libopus from decoding at a rate and channel count it defines.
    let decoder = Decoder::new(RATE, Channels::Stereo).map_err(decode_failed)?;

    Ok(Box::new(OpusSource {
        packets,
        decoder,
        stream,
        pre_skip: head.pre_skip,
        decoded: 0,
        samples: vec![0; MAX_FRAME * usize::from(CHANNELS)],
    }))
}

/// One logical Opus stream, decoded packet by packet.
struct OpusSource {
    /// The packets of the physical stream the input holds, this one's among them.
    packets: Packets,
    /// libopus, set up for the one shape every source hands out.
    decoder: Decoder,
    /// Which logical stream is this one: an Ogg file may carry several at once, and this is the
    /// one whose head was read.
    stream: u32,
    /// How many samples per channel the head asked to have thrown away before the audio proper.
    pre_skip: u64,
    /// How many samples per channel have been decoded, trimmed or not — which is where the next
    /// packet falls in the count a granule position is in.
    decoded: u64,
    /// The buffer every packet is decoded into, as long as the longest packet Opus defines.
    samples: Vec<i16>,
}

impl AudioSource for OpusSource {
    /// Always 48 kHz stereo: Opus is defined at the one rate, and the decoder was asked for the
    /// two channels whatever the stream holds.
    fn spec(&self) -> SourceSpec {
        SourceSpec {
            sample_rate: RATE,
            channels: CHANNELS,
        }
    }

    /// Nothing: what an Opus stream says about itself is in the comment header this does not read.
    fn metadata(&mut self) -> SourceMetadata {
        SourceMetadata::default()
    }

    fn next_block(&mut self) -> Result<Option<Vec<i16>>, DecodeError> {
        loop {
            let Some(packet) = self.packets.read_packet().map_err(stream_error)? else {
                return Ok(None);
            };
            // A file may carry the packets of several streams in turn, and the decoder was set up
            // for one of them.
            if packet.stream_serial() != self.stream {
                continue;
            }

            let block = self.decode(&packet)?;
            // A packet that is nothing but pre-skip, or nothing but the padding behind the end of
            // the recording, is a packet that decoded to none of it.
            if block.is_empty() {
                continue;
            }

            return Ok(Some(block));
        }
    }
}

impl OpusSource {
    /// The recording one packet holds, decoded and interleaved.
    ///
    /// What comes back is what the *file* states as audio: the samples in front of the pre-skip
    /// and behind the granule position the stream ends on are the encoder's own, and are dropped
    /// here rather than handed on as the silence they are.
    fn decode(&mut self, packet: &Packet) -> Result<Vec<i16>, DecodeError> {
        // What the packet says it holds, which is also whether it holds anything at all: libopus
        // reads a packet of no bytes as one that went missing on the way and invents audio to
        // cover the gap, and a file stating one is broken rather than lossy.
        self.decoder
            .get_nb_samples(&packet.data)
            .map_err(decode_failed)?;

        let frames = self
            .decoder
            .decode(&packet.data, &mut self.samples, false)
            .map_err(decode_failed)?;
        let at = self.decoded;
        // A packet decodes into the buffer or not at all, and that is 120 ms of the samples this
        // is counting.
        self.decoded = at.saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));

        // Only the last page of a stream states where the recording stops; every page before it
        // states how far the stream plays, which is what the packets on it decoded to anyway.
        let plays = self.pre_skip..end_of(packet);
        let kept = kept(frames, at, &plays);

        Ok(self
            .samples
            .get(kept.start * usize::from(CHANNELS)..kept.end * usize::from(CHANNELS))
            .unwrap_or_default()
            .to_vec())
    }
}

/// What the packet opening an Opus stream states that decoding it depends on.
struct OpusHead {
    /// How many samples per channel the encoder asks to have thrown away before the audio proper.
    pre_skip: u64,
}

impl OpusHead {
    /// What `packet` states, or `None` when it is no head this build can decode a stream from.
    ///
    /// RFC 7845 states the head as the magic, a version, the channel count, the pre-skip, the rate
    /// the stream was encoded from, an output gain and the channel mapping family — and behind
    /// that, for every family but the first, the table mapping the streams onto the channels.
    ///
    /// Three of those are read. The version's upper four bits are its major version, and a stream
    /// of one this does not know is one whose head may say something else entirely. The mapping
    /// family says how many Opus streams the channels come out of, and anything but family 0 —
    /// which is the one mono and stereo state — takes a decoder per stream and a table to place
    /// them, which is not what an audiobook is authored as. The pre-skip is what is *used*.
    ///
    /// The channel count is deliberately not: libopus decodes what its packets hold into the
    /// channels the decoder was set up for, so a stream of one channel and a stream of two both
    /// come out as the stereo every source here hands out.
    fn of(packet: &[u8]) -> Option<Self> {
        /// How far behind the magic the version sits.
        const VERSION_AT: usize = 0;
        /// How far behind it the pre-skip sits, which is two bytes wide and little-endian.
        const PRE_SKIP_AT: usize = 2;
        /// How far behind it the channel mapping family sits.
        const MAPPING_FAMILY_AT: usize = 10;
        /// The mapping family a stream of one or two plain channels states.
        const PLAIN_CHANNELS: u8 = 0;

        let head = packet.strip_prefix(HEAD_MAGIC)?;

        if head.get(VERSION_AT)? >> 4 != 0 || *head.get(MAPPING_FAMILY_AT)? != PLAIN_CHANNELS {
            return None;
        }

        let pre_skip = u16::from_le_bytes([*head.get(PRE_SKIP_AT)?, *head.get(PRE_SKIP_AT + 1)?]);

        Some(Self {
            pre_skip: u64::from(pre_skip),
        })
    }
}

/// Which frames of a packet of `frames` frames are the recording.
///
/// `at` is where the packet's first frame falls in the count the stream's granule positions are
/// in, and `plays` is the part of that count the file states as audio. What comes back is where
/// the two meet, counted from the packet's own start — an empty range when they do not meet at
/// all, which is what a packet of nothing but pre-skip or nothing but padding decodes to.
///
/// A stream stating an end in front of its own pre-skip states no audio rather than a range that
/// runs backwards.
fn kept(frames: usize, at: u64, plays: &Range<u64>) -> Range<usize> {
    let of_this_packet = |count: u64| {
        usize::try_from(count.saturating_sub(at))
            .unwrap_or(usize::MAX)
            .min(frames)
    };

    let start = of_this_packet(plays.start);

    start..of_this_packet(plays.end).max(start)
}

/// Where the recording a packet is part of stops, in the count the granule positions are in.
///
/// Only the page a stream ends on states that: its granule position is where the last packet stops
/// being audio, which is how the padding an encoder filled its final packet with is left out. Any
/// other packet is bounded by nothing — a stream that states no end at all, because it was cut
/// short or written by a muxer that never closed it, is handed out as far as it decodes.
fn end_of(packet: &Packet) -> u64 {
    if packet.last_in_stream() {
        packet.absgp_page()
    } else {
        u64::MAX
    }
}

/// Whether these first bytes of an input open an Ogg-Opus stream.
///
/// An Ogg file starts with a page, and RFC 7845 puts the `OpusHead` packet alone in the first page
/// of an Opus stream — so the magic stands behind that page's header and its lacing table, and the
/// byte counting the lacing values is what says where. Ogg files of every other codec state their
/// own magic in the same place and go to the demuxer that reads them.
fn opens_an_opus_stream(prefix: &[u8]) -> bool {
    if !prefix.starts_with(CAPTURE_PATTERN) {
        return false;
    }
    let Some(segments) = prefix.get(SEGMENT_COUNT_AT) else {
        return false;
    };

    prefix
        .get(PAGE_HEADER_LEN + usize::from(*segments)..)
        .is_some_and(|packet| packet.starts_with(HEAD_MAGIC))
}

/// As many of the input's bytes as `buffer` takes, or as many as there are.
///
/// # Errors
///
/// [`io::Error`] as the input reports it. What was read before that is not handed back, since a
/// sniff on half a read is one on bytes that may go on differently.
fn fill(source: &mut dyn MediaSource, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;

    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match source.read(rest)? {
            0 => break,
            read => filled += read,
        }
    }

    Ok(filled)
}

/// The next packet of the physical stream, where a stream that has ended is one that never stated
/// the headers it opens with.
///
/// # Errors
///
/// Whatever [`open_error`] makes of a read that failed, and
/// [`DecodeError::UnsupportedFormat`] when the packet is not there at all.
fn next_packet(packets: &mut Packets) -> Result<Packet, DecodeError> {
    packets
        .read_packet()
        .map_err(open_error)?
        .ok_or(DecodeError::UnsupportedFormat)
}

/// The next packet of the logical stream `stream`, with every other stream's passed over.
///
/// A file may carry several streams side by side, and then the packet behind a stream's first is
/// not the file's second packet — the pages opening the other streams stand in between.
///
/// # Errors
///
/// Whatever [`next_packet`] states, including the file running out before the stream states
/// another packet.
fn next_packet_of(packets: &mut Packets, stream: u32) -> Result<Packet, DecodeError> {
    loop {
        let packet = next_packet(packets)?;

        if packet.stream_serial() == stream {
            return Ok(packet);
        }
    }
}

/// What a failure while opening a stream means.
///
/// The input was sniffed as Ogg-Opus before anything got here, so what is left to go wrong is the
/// stream not being what its first page claimed: a page that does not hold together, a checksum
/// that does not match what it carries, bytes that run out in front of the headers. A read that
/// fails for some other reason is the input itself being unreadable, which is the caller's file
/// rather than the caller's audio.
fn open_error(err: OggReadError) -> DecodeError {
    match err {
        OggReadError::ReadError(err) if err.kind() != ErrorKind::UnexpectedEof => {
            DecodeError::Io(err)
        }
        _ => DecodeError::UnsupportedFormat,
    }
}

/// What a failure while decoding means.
///
/// The end of a stream is not one of these: the reader states that as a packet that is not there.
/// So bytes that run out here are a file that broke off mid-page, which would silently truncate a
/// book if it were reported as the end, and every other read failure is the input going away
/// mid-decode. What is left is a stream that does not hold together, or audio that does not
/// decode, and both are the audio's problem rather than the file's.
fn stream_error(err: OggReadError) -> DecodeError {
    match err {
        OggReadError::ReadError(err) => DecodeError::Io(err),
        err => decode_failed(err),
    }
}

/// A failure of the framing or the codec, as the one thing it amounts to: what the input holds
/// does not decode.
fn decode_failed(err: impl std::error::Error + Send + Sync + 'static) -> DecodeError {
    DecodeError::Decode(Box::new(err))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{kept, opens_an_opus_stream, OpusHead, Range, HEAD_MAGIC, PAGE_HEADER_LEN};

    /// The head every case here starts from: version 1, stereo, a pre-skip of 312, 48 kHz, no
    /// gain, and the channel mapping a plain stereo stream states.
    const HEAD: [u8; 11] = [1, 2, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, 0];

    /// An `OpusHead` packet of `head` behind the magic.
    fn head_packet(head: &[u8]) -> Vec<u8> {
        let mut packet = HEAD_MAGIC.to_vec();
        packet.extend_from_slice(head);

        packet
    }

    /// The first bytes of a page whose lacing table states `segments` values, and which carries
    /// `packet` behind it.
    fn page(segments: usize, packet: &[u8]) -> Vec<u8> {
        let mut page = b"OggS".to_vec();
        page.resize(PAGE_HEADER_LEN - 1, 0);
        page.push(u8::try_from(segments).unwrap());
        page.resize(PAGE_HEADER_LEN + segments, 1);
        page.extend_from_slice(packet);

        page
    }

    #[test]
    fn a_first_page_carrying_the_opus_magic_opens_an_opus_stream() {
        assert!(opens_an_opus_stream(&page(1, HEAD_MAGIC)));
        // The magic sits behind the whole lacing table, however long the page states it is.
        assert!(opens_an_opus_stream(&page(3, HEAD_MAGIC)));
    }

    #[test]
    fn bytes_that_are_not_a_page_at_all_open_nothing() {
        assert!(!opens_an_opus_stream(b""));
        assert!(!opens_an_opus_stream(b"RIFF"));
        // A capture pattern and nothing behind it: no byte states how long the lacing table is.
        assert!(!opens_an_opus_stream(b"OggS"));
    }

    #[test]
    fn a_page_of_another_codec_opens_no_opus_stream() {
        // What a Vorbis stream states where an Opus one states its own magic.
        assert!(!opens_an_opus_stream(&page(1, b"\x01vorbis")));
    }

    #[test]
    fn the_magic_is_looked_for_behind_the_whole_lacing_table() {
        // A page stating three lacing values, carrying the magic where a page stating none would.
        let mut misplaced = page(3, HEAD_MAGIC);
        misplaced.drain(PAGE_HEADER_LEN..PAGE_HEADER_LEN + 3);

        assert!(!opens_an_opus_stream(&misplaced));
    }

    #[test]
    fn a_page_that_breaks_off_in_front_of_its_first_packet_opens_nothing() {
        let page = page(1, HEAD_MAGIC);

        for len in 0..page.len() {
            assert!(
                !opens_an_opus_stream(page.get(..len).unwrap()),
                "{len} bytes of a page opened a stream"
            );
        }
    }

    #[test]
    fn a_head_states_the_pre_skip_its_encoder_asked_for() {
        let head = OpusHead::of(&head_packet(&HEAD)).expect("a head this build decodes from");

        assert_eq!(head.pre_skip, 312);
    }

    #[test]
    fn a_packet_that_is_not_a_head_states_nothing() {
        assert!(OpusHead::of(b"OpusTags").is_none());
        // The magic and nothing behind it, and a head cut short of its channel mapping — which is
        // the last of the three fields read out of one.
        assert!(OpusHead::of(HEAD_MAGIC).is_none());
        assert!(OpusHead::of(&head_packet(HEAD.get(..10).unwrap())).is_none());
    }

    #[test]
    fn a_head_of_a_major_version_this_does_not_know_states_nothing() {
        let mut head = HEAD;
        // The upper four bits are the major version; the lower four are the minor one, which a
        // decoder is meant to read straight past.
        head[0] = 0x0f;
        assert!(OpusHead::of(&head_packet(&head)).is_some());

        head[0] = 0x10;
        assert!(OpusHead::of(&head_packet(&head)).is_none());
    }

    #[test]
    fn a_head_whose_channels_take_more_than_one_decoder_states_nothing() {
        let mut head = HEAD;
        head[10] = 1;

        assert!(OpusHead::of(&head_packet(&head)).is_none());
    }

    #[test]
    fn a_packet_inside_what_a_stream_plays_is_kept_whole() {
        assert_eq!(kept(960, 1_920, &(312..96_312)), 0..960);
    }

    #[test]
    fn the_pre_skip_comes_off_the_packets_it_reaches_into() {
        // The first packet of a stream whose head states 312 samples of pre-skip, and the second
        // one, which the pre-skip does not reach.
        assert_eq!(kept(960, 0, &(312..96_312)), 312..960);
        assert_eq!(kept(960, 960, &(312..96_312)), 0..960);
    }

    #[test]
    fn a_packet_the_pre_skip_covers_whole_is_kept_at_none_of_it() {
        assert_eq!(kept(960, 0, &(3_000..96_312)), 960..960);
    }

    #[test]
    fn the_padding_behind_the_end_of_a_stream_comes_off_its_last_packet() {
        // 96 960 samples decoded against a stream that plays to 96 312: the last packet holds
        // 648 samples of the recording and 312 of what the encoder filled it out with.
        assert_eq!(kept(960, 96_000, &(312..96_312)), 0..312);
        // And a packet entirely behind the end holds none of it.
        assert_eq!(kept(960, 96_312, &(312..96_312)), 0..0);
    }

    #[test]
    fn a_stream_that_ends_in_front_of_its_own_pre_skip_states_no_audio() {
        // A range that runs backwards is one no slice can be taken from, so it comes back empty.
        let backwards = Range {
            start: 312,
            end: 100,
        };

        let kept = kept(960, 0, &backwards);

        assert!(kept.is_empty(), "a backwards stream kept {kept:?}");
    }
}
