//! The pages a TAF's audio region is written as, and the two packets its Opus stream opens with.
//!
//! [`PageBuilder`] is the writing half of [`PageView`](super::PageView): packets go in until the
//! page has no room left for another, and its bytes come out with the checksum a reader checks
//! them against. [`opus_head`] and [`opus_tags`] build the two packets the stream starts with,
//! both at the lengths a TAF fixes them at — 19 bytes and 436 — which is what leaves the first
//! audio page exactly the room it needs to close the block it shares with them.
//!
//! Nothing here decides how long a packet should be or where a page ends. Sizing packets so pages
//! land on block boundaries is the business of whoever writes the file, and `FORMAT.md` in this
//! crate spells out the arithmetic that takes.
//!
//! The two packets are fixed-length arrays and need no allocator, so a file's stream headers can
//! be built anywhere. A page is as long as its packets make it, so [`PageBuilder`] hands one over
//! as a `Vec` and is there only with the `alloc` feature.

use core::fmt;
#[cfg(feature = "alloc")]
use core::iter;

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use super::{
    crc::crc32, CHECKSUM_AT, CHECKSUM_LEN, CONTINUES, FLAG_FIRST, FLAG_LAST, HEADER_LEN, MAGIC,
    PAGE_LEN, VERSION,
};

/// The bytes one lacing value states at most.
///
/// A value below this ends its packet, and 255 says the segment carries on into the next one — so
/// a packet of exactly this many bytes costs two lacing values, the second of them zero.
pub(crate) const SEGMENT_LEN: usize = 255;

/// The lacing values one page states at most: what the single byte counting them can say.
pub(crate) const MAX_SEGMENTS: usize = 255;

/// The longest packet a page's lacing table describes at all: 254 full segments and one more value
/// below 255 to end it.
const MAX_PACKET_LEN: usize = MAX_SEGMENTS * SEGMENT_LEN - 1;

/// The bytes an `OpusHead` packet occupies.
pub const OPUS_HEAD_LEN: usize = 19;

/// The bytes an `OpusTags` packet occupies in a TAF, whatever it says.
pub const OPUS_TAGS_LEN: usize = 436;

/// The pre-skip every TAF states: the samples a decoder throws away before the audio proper.
///
/// teddycloud writes this number rather than asking its encoder for one, and it is libopus's own
/// 6.5 ms of lookahead at 48 kHz. Writing anything else makes a file that no longer matches the
/// ones a box already plays.
pub const OPUS_PRE_SKIP: u16 = 312;

/// The magic an `OpusHead` packet starts with.
pub(crate) const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";

/// The magic an `OpusTags` packet starts with.
pub(crate) const OPUS_TAGS_MAGIC: &[u8; 8] = b"OpusTags";

/// The one version of the Opus header a TAF states.
const OPUS_VERSION: u8 = 1;

/// The channels a TAF's audio carries.
const CHANNELS: u8 = 2;

/// The rate the audio was handed to the encoder at, which the header states for a decoder's
/// benefit rather than to be decoded at.
const SAMPLE_RATE: u32 = 48_000;

/// The bytes a length occupies in an `OpusTags` packet: four, little-endian, in front of every
/// string.
const TAGS_LEN_LEN: usize = 4;

/// The comment that fills an `OpusTags` packet out to its fixed length.
const PAD_COMMENT: &[u8; 4] = b"pad=";

/// The byte teddycloud fills the room behind the `pad=` comment's name with.
const PAD_FILL: u8 = b'0';

/// Why a piece of a TAF's audio region could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildError {
    /// The page has no room left for the packet handed to `PageBuilder::push_packet` — either in
    /// the bytes a page occupies or in the lacing values it may state.
    PageFull,
    /// The packet handed to `PageBuilder::push_packet` is longer than a page's lacing table
    /// describes, whatever else that page holds.
    PacketTooLarge,
    /// The vendor string and comments handed to [`opus_tags`] leave no room for the `pad=` comment
    /// that fills the packet out to its fixed length.
    TagsTooLong,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::PageFull => f.write_str("an Ogg page has no room left for another packet"),
            Self::PacketTooLarge => write!(
                f,
                "a packet is longer than the {MAX_PACKET_LEN} bytes an Ogg page's lacing table describes"
            ),
            Self::TagsTooLong => {
                write!(f, "OpusTags content does not fit a {OPUS_TAGS_LEN}-byte packet")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BuildError {}

/// One Ogg page, built packet by packet.
///
/// A page is a header, a lacing table saying how its body divides into packets, and the body
/// itself. Packets are pushed until the page has no room for another —
/// [`body_capacity_left`](PageBuilder::body_capacity_left) says beforehand how much room one more
/// has — and [`finish`](PageBuilder::finish) hands over the bytes.
///
/// Nothing pads the page: it comes out exactly as long as its packets make it. That a TAF's
/// aligned pages measure [`PAGE_LEN`] is something the writer arranges by sizing the last packet
/// of every page to land there.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct PageBuilder {
    granule_position: u64,
    serial: u32,
    sequence: u32,
    flags: u8,
    lacing: Vec<u8>,
    body: Vec<u8>,
}

#[cfg(feature = "alloc")]
impl PageBuilder {
    /// Starts an empty page of the stream `serial`, at position `sequence`.
    ///
    /// Every page of a TAF states the file's audio id as its serial number, and its sequence
    /// numbers run from zero without a gap.
    #[must_use]
    pub const fn new(serial: u32, sequence: u32) -> Self {
        Self {
            granule_position: 0,
            serial,
            sequence,
            flags: 0,
            lacing: Vec::new(),
            body: Vec::new(),
        }
    }

    /// States the granule position: the sample the audio this page carries plays up to.
    ///
    /// A page that states none is a page that ends no packet; a TAF's two header pages state zero
    /// and its audio pages count in samples of one channel.
    pub fn granule_position(&mut self, granule: u64) {
        self.granule_position = granule;
    }

    /// States which end of its stream the page is: `first` sets the BOS flag, `last` the EOS flag.
    ///
    /// Both are stated together, so a second call restates both rather than adding to the first. A
    /// TAF states `first` on page 0 and nothing anywhere else — teddycloud never ends a stream, so
    /// no page of a TAF carries EOS.
    pub fn flags(&mut self, first: bool, last: bool) {
        let mut flags = 0;

        if first {
            flags |= FLAG_FIRST;
        }
        if last {
            flags |= FLAG_LAST;
        }

        self.flags = flags;
    }

    /// Adds `packet` to the page, lacing values and all.
    ///
    /// The packet is taken whole or not at all: a page that cannot hold it is left exactly as it
    /// was, for the caller to finish and start the next one with.
    ///
    /// # Errors
    ///
    /// - [`BuildError::PacketTooLarge`] if the packet is longer than the 65 024 bytes a page's 255
    ///   lacing values describe. This is about the packet alone, so it is answered before the page
    ///   is even looked at.
    /// - [`BuildError::PageFull`] if the packet and the lacing values describing it do not fit
    ///   what [`body_capacity_left`](PageBuilder::body_capacity_left) says is left.
    pub fn push_packet(&mut self, packet: &[u8]) -> Result<(), BuildError> {
        if packet.len() > MAX_PACKET_LEN {
            return Err(BuildError::PacketTooLarge);
        }

        if packet_cost(packet.len()) > self.body_capacity_left() {
            return Err(BuildError::PageFull);
        }

        lace(&mut self.lacing, packet.len());
        self.body.extend_from_slice(packet);

        Ok(())
    }

    /// Returns how many bytes of this page one further packet may occupy, its lacing values
    /// counted in.
    ///
    /// A packet of `b` bytes occupies `b + b / 255 + 1` bytes of a page: itself, one lacing value
    /// for every full 255-byte segment, and one more that ends it.
    /// [`push_packet`](PageBuilder::push_packet) takes exactly the packets whose occupancy is this
    /// number or less — and which are no longer than the 65 024 bytes a lacing table describes at
    /// all.
    ///
    /// Both of a page's limits are in this number: the [`PAGE_LEN`] bytes a page occupies, header
    /// and lacing table included, and the 255 lacing values it may state.
    ///
    /// Not every number of bytes has a packet that fills it exactly. Room for 512 takes a packet
    /// of 509 bytes at most — 510 would need a second lacing value and overshoot — and the byte
    /// that leaves over is the writer's problem to spend, by shortening an earlier packet or
    /// padding this one.
    #[must_use]
    pub fn body_capacity_left(&self) -> usize {
        let spent = HEADER_LEN
            .saturating_add(self.lacing.len())
            .saturating_add(self.body.len());
        let page_room = PAGE_LEN.saturating_sub(spent);

        // What is left of the lacing table bounds a packet too: `n` values state a packet of at
        // most `255 * n - 1` bytes — one value below 255 has to end it — which with those values
        // themselves occupies `256 * n - 1` bytes.
        let values_left = MAX_SEGMENTS.saturating_sub(self.lacing.len());
        let lacing_room = values_left
            .saturating_mul(SEGMENT_LEN + 1)
            .saturating_sub(1);

        page_room.min(lacing_room)
    }

    /// Hands over the page: its header, its lacing table, its body, and the checksum over all
    /// three.
    ///
    /// Nothing is padded — the page is as long as its packets made it — and nothing is stated
    /// about what follows it.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        // A page never grows past a block, so this is the only allocation the page takes.
        let mut page = Vec::with_capacity(PAGE_LEN);

        page.extend_from_slice(MAGIC);
        page.push(VERSION);
        page.push(self.flags);
        page.extend_from_slice(&self.granule_position.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        // The checksum is summed over a page that states zeros here, and states the sum instead
        // once it is known.
        page.extend_from_slice(&[0; CHECKSUM_LEN]);
        // The page never took on more lacing values than the byte counting them states.
        page.push(u8::try_from(self.lacing.len()).unwrap_or(CONTINUES));
        page.extend_from_slice(&self.lacing);
        page.extend_from_slice(&self.body);

        let checksum = crc32(&page).to_le_bytes();
        for (slot, &byte) in page.iter_mut().skip(CHECKSUM_AT).zip(&checksum) {
            *slot = byte;
        }

        page
    }
}

/// What a packet of `len` bytes occupies on a page: itself, one lacing value for every full
/// segment, and one more that ends it.
///
/// Callers check `len` against [`MAX_PACKET_LEN`] first, so this stays far from overflowing.
#[cfg(feature = "alloc")]
pub(crate) const fn packet_cost(len: usize) -> usize {
    len + len / SEGMENT_LEN + 1
}

/// Writes the lacing values that describe a packet of `len` bytes.
///
/// A 255 for every full segment, then what is left over — which is a value below 255, and that is
/// what says the packet ends there. So a packet of exactly 255 bytes is laced `255, 0`, and a
/// packet of no bytes at all is laced `0`.
#[cfg(feature = "alloc")]
fn lace(lacing: &mut Vec<u8>, len: usize) {
    lacing.extend(iter::repeat_n(CONTINUES, len / SEGMENT_LEN));
    // A remainder is smaller than what it was divided by, so this always converts.
    lacing.push(u8::try_from(len % SEGMENT_LEN).unwrap_or(CONTINUES));
}

/// Builds the `OpusHead` packet an Opus stream opens with: version 1, two channels handed in at
/// 48 kHz, no output gain, and the channel mapping stereo streams state.
///
/// `pre_skip` is how many samples a decoder throws away before the audio proper;
/// [`OPUS_PRE_SKIP`] is what a TAF states.
#[must_use]
pub fn opus_head(pre_skip: u16) -> [u8; OPUS_HEAD_LEN] {
    let mut packet = [0; OPUS_HEAD_LEN];
    let mut writer = PacketWriter::new(&mut packet);

    writer.put(OPUS_HEAD_MAGIC);
    writer.put(&[OPUS_VERSION, CHANNELS]);
    writer.put(&pre_skip.to_le_bytes());
    writer.put(&SAMPLE_RATE.to_le_bytes());
    // The two bytes of output gain and the one of channel mapping family are the zeros the packet
    // started as: no gain, and the one mapping a plain stereo stream states.

    packet
}

/// Builds the `OpusTags` packet that names who wrote the stream and what about it is worth saying.
///
/// The packet is always [`OPUS_TAGS_LEN`] bytes, whatever it says: `vendor` and `comments` are
/// written as handed in, and a last `pad=` comment takes up whatever room is left over. That fixed
/// length is what a TAF is built on — the two pages carrying the Opus headers come to 512 bytes
/// together, leaving the first audio page exactly the 3584 it needs to close the first block.
///
/// Comments are the `NAME=value` strings a Vorbis comment block holds, and nothing here checks
/// that they look like one.
///
/// # Errors
///
/// [`BuildError::TagsTooLong`] if the vendor string and the comments leave the `pad=` comment less
/// room than its own name and length take: 428 bytes of content is the most that fits, counting
/// the four-byte length in front of every string.
pub fn opus_tags(vendor: &str, comments: &[&str]) -> Result<[u8; OPUS_TAGS_LEN], BuildError> {
    // The magic, the vendor string behind its length, and the count of the comments behind it.
    let content = comments.iter().fold(
        OPUS_TAGS_MAGIC
            .len()
            .saturating_add(TAGS_LEN_LEN)
            .saturating_add(vendor.len())
            .saturating_add(TAGS_LEN_LEN),
        |content, comment| {
            content
                .saturating_add(TAGS_LEN_LEN)
                .saturating_add(comment.len())
        },
    );

    // What is left for the padding comment once its own length is paid for, which teddycloud
    // works out the same way. It has to hold at least the comment's name.
    let padding = match OPUS_TAGS_LEN.checked_sub(content.saturating_add(TAGS_LEN_LEN)) {
        Some(padding) if padding >= PAD_COMMENT.len() => padding,
        _ => return Err(BuildError::TagsTooLong),
    };

    let mut packet = [PAD_FILL; OPUS_TAGS_LEN];
    let mut writer = PacketWriter::new(&mut packet);

    writer.put(OPUS_TAGS_MAGIC);
    writer.put_len(vendor.len());
    writer.put(vendor.as_bytes());
    // The padding is a comment like any other, and counts as one.
    writer.put_len(comments.len().saturating_add(1));
    for comment in comments {
        writer.put_len(comment.len());
        writer.put(comment.as_bytes());
    }
    writer.put_len(padding);
    writer.put(PAD_COMMENT);
    // Behind the name, the padding is the '0' bytes the packet started as, which is what
    // teddycloud leaves there.

    Ok(packet)
}

/// The packet being written, and how far into it the writing has come.
struct PacketWriter<'a> {
    packet: &'a mut [u8],
    pos: usize,
}

impl<'a> PacketWriter<'a> {
    /// Starts at the front of `packet`.
    const fn new(packet: &'a mut [u8]) -> Self {
        Self { packet, pos: 0 }
    }

    /// Copies as much of `bytes` as the packet still holds in at the cursor, and steps past them.
    ///
    /// Both packets here are sized before a byte of them is written, so the packet always does
    /// hold them; pairing the two off against each other rather than trusting that is what keeps a
    /// packet from ever being written past its end.
    fn put(&mut self, bytes: &[u8]) {
        for (slot, &byte) in self.packet.iter_mut().skip(self.pos).zip(bytes) {
            *slot = byte;
        }

        self.pos = self.pos.saturating_add(bytes.len());
    }

    /// Writes the length in front of a string, which an `OpusTags` packet states as four
    /// little-endian bytes.
    ///
    /// Every length written here is one the packet has room for, so none comes anywhere near this
    /// wide; clamping rather than wrapping keeps an impossible one from passing for a plausible
    /// one.
    fn put_len(&mut self, len: usize) {
        self.put(&u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes());
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::super::{PageError, PageView};
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    const GOLDEN: &[u8] = include_bytes!("../../tests/fixtures/golden-sine.taf");

    /// The two pages carrying the Opus headers, which is what the golden file's audio region
    /// starts with: page 0 at 4096, page 1 behind it, and the first audio page at 4608.
    const HEADER_PAGES: core::ops::Range<usize> = 4096..4608;

    /// The serial number every page of the golden file states: the file's audio id.
    const SERIAL: u32 = 444_913_029;

    /// The vendor string teddycloud wrote into the golden file.
    const VENDOR: &str = "teddyCloud";

    /// The one comment it wrote besides the padding.
    const COMMENT: &str =
        "version=TeddyCloud v0.6.2 (203f12d) - 2024-10-26 18:14:34 +0000 ubuntu linux-x86_64(64)";

    /// A page of `packets`, built the way a writer builds one.
    fn page(sequence: u32, granule: u64, first: bool, packets: &[&[u8]]) -> Vec<u8> {
        let mut builder = PageBuilder::new(SERIAL, sequence);
        builder.granule_position(granule);
        builder.flags(first, false);

        for packet in packets {
            builder.push_packet(packet).unwrap();
        }

        builder.finish()
    }

    /// The packets a built page carries, bounded a step past the 255 a page can hold.
    fn packet_lens(page: &[u8]) -> Vec<usize> {
        PageView::parse(page)
            .unwrap()
            .packets()
            .take(256)
            .map(<[u8]>::len)
            .collect()
    }

    /// What a packet of `len` bytes occupies on a page, spelled out from RFC 3533 rather than
    /// taken from the builder: the packet itself, one lacing value for every full 255-byte
    /// segment, and one more that ends it.
    fn rfc_packet_cost(len: usize) -> usize {
        len + len / 255 + 1
    }

    /// The largest packet that fits in `room` bytes with its lacing values, worked out by trying
    /// every size rather than by the arithmetic the builder does.
    fn largest_packet(room: usize) -> usize {
        (0..room)
            .rev()
            .find(|&len| rfc_packet_cost(len) <= room)
            .unwrap()
    }

    /// Reads an `OpusTags` packet back the way a player reads one: the vendor string, then every
    /// comment, each behind its own four-byte little-endian length.
    fn read_tags(packet: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        assert!(packet.starts_with(OPUS_TAGS_MAGIC));

        let len_at = |at: usize| {
            usize::try_from(u32::from_le_bytes(packet[at..at + 4].try_into().unwrap())).unwrap()
        };
        let string = |at: &mut usize| {
            let len = len_at(*at);
            let string = packet[*at + 4..*at + 4 + len].to_vec();
            *at += 4 + len;

            string
        };

        let mut at = OPUS_TAGS_MAGIC.len();
        let vendor = string(&mut at);
        let count = len_at(at);
        at += 4;
        let comments = (0..count).map(|_| string(&mut at)).collect();

        assert_eq!(at, packet.len(), "the packet ends where its comments end");

        (vendor, comments)
    }

    #[test]
    fn builds_a_page_the_reader_reads_back() {
        let mut builder = PageBuilder::new(SERIAL, 7);
        builder.granule_position(31_680);
        builder.push_packet(&[1, 2, 3]).unwrap();
        builder.push_packet(&[0xa5; 300]).unwrap();
        let page = builder.finish();

        // Parsing checks the checksum, so a view at all is a page whose bytes add up.
        let view = PageView::parse(&page).unwrap();

        assert_eq!(view.serial(), SERIAL);
        assert_eq!(view.sequence(), 7);
        assert_eq!(view.granule_position(), 31_680);
        assert!(!view.is_first());
        assert!(!view.is_last());
        assert_eq!(view.total_len(), page.len());
        assert_eq!(
            view.packets().take(256).collect::<Vec<_>>(),
            [&[1, 2, 3][..], &[0xa5; 300][..]]
        );

        // 27 header bytes, three lacing values — one for the short packet, two for the one that
        // spans a full segment — and the 303 bytes of packet behind them.
        assert_eq!(page.len(), 27 + 3 + 303);
    }

    #[test]
    fn builds_the_opus_head_packet_the_golden_file_states() {
        let stated = PageView::parse(&GOLDEN[HEADER_PAGES])
            .unwrap()
            .packets()
            .next()
            .unwrap();

        assert_eq!(OPUS_PRE_SKIP, 312);
        assert_eq!(opus_head(OPUS_PRE_SKIP).as_slice(), stated);

        // The pre-skip is the only thing about the packet a caller chooses, and it is the
        // little-endian pair ten bytes in.
        let none = opus_head(0);
        assert_eq!(&none[10..12], &[0, 0]);
        assert_eq!(&none[..10], &stated[..10]);
        assert_eq!(&none[12..], &stated[12..]);
    }

    #[test]
    fn builds_the_two_header_pages_the_golden_file_starts_with() {
        let head = page(0, 0, true, &[&opus_head(OPUS_PRE_SKIP)]);
        let tags = page(1, 0, false, &[&opus_tags(VENDOR, &[COMMENT]).unwrap()]);

        // 27 header bytes plus one lacing value plus 19, and 27 plus two lacing values plus 436:
        // the 512 bytes that leave the first audio page the rest of the block.
        assert_eq!(head.len(), 47);
        assert_eq!(tags.len(), 465);

        let mut both = head;
        both.extend_from_slice(&tags);

        assert_eq!(both.len(), 512);
        assert_eq!(both, GOLDEN[HEADER_PAGES]);
    }

    #[test]
    fn fills_the_tags_packet_out_to_its_fixed_length_whatever_it_says() {
        let cases: [(&str, &[&str]); 4] = [
            ("", &[]),
            ("taffle", &["encoder=taffle"]),
            (VENDOR, &[COMMENT]),
            ("v", &["x=1", "y=2", "z=3"]),
        ];

        for (vendor, comments) in cases {
            let packet = opus_tags(vendor, comments).unwrap();
            let (read_vendor, read_comments) = read_tags(&packet);

            assert_eq!(packet.len(), OPUS_TAGS_LEN, "{vendor}");
            assert_eq!(read_vendor, vendor.as_bytes(), "{vendor}");
            assert_eq!(read_comments.len(), comments.len() + 1, "{vendor}");

            for (read, comment) in read_comments.iter().zip(comments) {
                assert_eq!(read, comment.as_bytes(), "{vendor}");
            }

            // The last comment is the padding, and what it pads with is what teddycloud pads with.
            let padding = read_comments.last().unwrap();
            assert!(padding.starts_with(PAD_COMMENT), "{vendor}");
            assert!(
                padding[PAD_COMMENT.len()..].iter().all(|&b| b == PAD_FILL),
                "{vendor}"
            );
        }
    }

    #[test]
    fn refuses_tags_that_leave_no_room_for_the_padding_comment() {
        // What is left for a vendor string once the magic, the vendor's own length, the comment
        // count and the shortest possible padding comment — its length and the four bytes of its
        // name — are paid for.
        let longest = OPUS_TAGS_LEN - 8 - 4 - 4 - 4 - PAD_COMMENT.len();
        let vendor = String::from_utf8(vec![b'v'; longest]).unwrap();
        let (read_vendor, comments) = read_tags(&opus_tags(&vendor, &[]).unwrap());

        assert_eq!(longest, 412);
        assert_eq!(read_vendor.len(), longest);
        assert_eq!(comments, [PAD_COMMENT.to_vec()]);

        // One byte more of content, from either end, and the padding comment no longer fits.
        let one_too_many = String::from_utf8(vec![b'v'; longest + 1]).unwrap();
        assert_eq!(
            opus_tags(&one_too_many, &[]),
            Err(BuildError::TagsTooLong),
            "a vendor string one byte too long"
        );
        assert_eq!(
            opus_tags(VENDOR, &[&vendor]),
            Err(BuildError::TagsTooLong),
            "a comment that leaves the padding no room"
        );
    }

    #[test]
    fn takes_the_largest_packet_that_fits_and_no_more() {
        let mut builder = PageBuilder::new(SERIAL, 0);

        // An empty page has spent its 27 header bytes and nothing else.
        assert_eq!(builder.body_capacity_left(), PAGE_LEN - 27);
        assert_eq!(largest_packet(builder.body_capacity_left()), 4053);

        let largest = largest_packet(builder.body_capacity_left());
        assert_eq!(
            builder.clone().push_packet(&vec![0; largest + 1]),
            Err(BuildError::PageFull)
        );

        builder.push_packet(&vec![0; largest]).unwrap();

        // 27 header bytes, sixteen lacing values and 4053 of packet: the page is full to the byte.
        assert_eq!(builder.body_capacity_left(), 0);
        assert_eq!(builder.clone().push_packet(&[]), Err(BuildError::PageFull));
        assert_eq!(builder.finish().len(), PAGE_LEN);
    }

    #[test]
    fn takes_exactly_the_packets_its_capacity_says_it_takes() {
        // A page at every shape: empty, a byte in, a full segment in, and nearly full.
        for filler in [0, 1, 255, 3000] {
            let mut builder = PageBuilder::new(SERIAL, 0);
            builder.push_packet(&vec![0; filler]).unwrap();

            let room = builder.body_capacity_left();
            let largest = largest_packet(room);
            let spent = PAGE_LEN - room;

            assert_eq!(
                builder.clone().push_packet(&vec![0; largest + 1]),
                Err(BuildError::PageFull),
                "{filler} bytes in"
            );

            builder.push_packet(&vec![0; largest]).unwrap();

            assert_eq!(
                builder.body_capacity_left(),
                room - rfc_packet_cost(largest),
                "{filler} bytes in"
            );
            assert_eq!(
                builder.finish().len(),
                spent + rfc_packet_cost(largest),
                "{filler} bytes in"
            );
        }
    }

    #[test]
    fn leaves_the_room_the_format_document_works_out_for_a_page() {
        // FORMAT.md's worked example: a page with 512 bytes already spent on it has 3557 left once
        // its header is paid for, and teddycloud's own sizing puts the largest packet that fits at
        // 3543 — thirteen full segments, a fourteenth lacing value, and no byte left over.
        let mut builder = PageBuilder::new(SERIAL, 2);
        builder.push_packet(&[]).unwrap();
        builder.push_packet(&[0; 509]).unwrap();

        assert_eq!(builder.body_capacity_left(), 3557);
        assert_eq!(largest_packet(3557), 3543);

        builder.push_packet(&[0; 3543]).unwrap();

        assert_eq!(builder.body_capacity_left(), 0);
        assert_eq!(builder.finish().len(), PAGE_LEN);
    }

    #[test]
    fn runs_out_of_lacing_values_before_it_runs_out_of_room() {
        let mut builder = PageBuilder::new(SERIAL, 0);
        for _ in 0..MAX_SEGMENTS {
            builder.push_packet(&[7]).unwrap();
        }

        // Two bytes apiece is nowhere near the 4096 a page holds; what this page has spent is the
        // 255 lacing values its segment count can state.
        assert_eq!(builder.body_capacity_left(), 0);
        assert_eq!(builder.clone().push_packet(&[]), Err(BuildError::PageFull));

        let page = builder.finish();

        assert_eq!(page.len(), 27 + 255 + 255);
        assert_eq!(packet_lens(&page).len(), 255);
    }

    #[test]
    fn counts_the_lacing_value_that_ends_a_packet_against_the_page() {
        let mut builder = PageBuilder::new(SERIAL, 0);
        for _ in 0..MAX_SEGMENTS - 1 {
            builder.push_packet(&[7]).unwrap();
        }

        // One lacing value left over: 254 bytes it can state, 255 it cannot, because that packet
        // needs a second value to say where it ends.
        assert_eq!(builder.body_capacity_left(), 255);
        assert_eq!(
            builder.clone().push_packet(&[0; 255]),
            Err(BuildError::PageFull)
        );

        builder.push_packet(&[0; 254]).unwrap();

        assert_eq!(builder.body_capacity_left(), 0);
    }

    #[test]
    fn refuses_a_packet_no_page_describes_before_it_looks_at_the_room() {
        let mut builder = PageBuilder::new(SERIAL, 0);

        // 65 024 bytes is the most a lacing table describes. It does not fit the 4096 a page
        // occupies either, and the nearly empty page says so; one byte more is a packet no page
        // could ever hold, and that is what it is told, whatever room this page has.
        assert_eq!(
            builder.push_packet(&vec![0; MAX_PACKET_LEN]),
            Err(BuildError::PageFull)
        );
        assert_eq!(
            builder.push_packet(&vec![0; MAX_PACKET_LEN + 1]),
            Err(BuildError::PacketTooLarge)
        );
        assert_eq!(MAX_PACKET_LEN, 65_024);

        // Nothing was taken on the way.
        assert_eq!(builder.body_capacity_left(), PAGE_LEN - 27);
    }

    #[test]
    fn laces_a_packet_that_fills_a_segment_with_the_value_that_ends_it() {
        let page = page(0, 0, false, &[&[0xa5; 255]]);

        // The segment count and the two lacing values behind it: 255 says the segment carries on,
        // and the zero behind it says the packet ends there.
        assert_eq!(&page[26..29], &[2, 255, 0]);
        assert_eq!(packet_lens(&page), [255]);
    }

    #[test]
    fn takes_a_packet_of_no_bytes_at_all() {
        let page = page(0, 0, false, &[&[], &[1]]);
        let view = PageView::parse(&page).unwrap();

        assert_eq!(packet_lens(&page), [0, 1]);
        assert_eq!(page.len(), 27 + 2 + 1);

        // A page states no granule position until it is told one.
        assert_eq!(view.granule_position(), 0);
    }

    #[test]
    fn states_the_ends_of_the_stream_it_is_told_to() {
        for (first, last) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut builder = PageBuilder::new(SERIAL, 0);
            builder.flags(first, last);
            builder.push_packet(&[0]).unwrap();
            let page = builder.finish();
            let view = PageView::parse(&page).unwrap();

            assert_eq!(view.is_first(), first, "first {first}, last {last}");
            assert_eq!(view.is_last(), last, "first {first}, last {last}");
        }

        // Stating the flags again states both of them again rather than adding to what was said.
        let mut builder = PageBuilder::new(SERIAL, 0);
        builder.flags(true, true);
        builder.flags(false, false);
        builder.push_packet(&[0]).unwrap();
        let page = builder.finish();
        let view = PageView::parse(&page).unwrap();

        assert!(!view.is_first());
        assert!(!view.is_last());
    }

    #[test]
    fn states_a_checksum_over_the_bytes_of_the_whole_page() {
        let page = page(3, 2880, false, &[&[0xa5; 400]]);

        // Every part of the page is summed: the granule position, the serial, the sequence, the
        // checksum the page itself states, the lacing table and the body. A reader that finds one
        // of them changed says so.
        for byte in [6, 14, 18, 22, 27, page.len() - 1] {
            let mut broken = page.clone();
            broken[byte] ^= 0x01;

            assert_eq!(
                PageView::parse(&broken).unwrap_err(),
                PageError::BadCrc,
                "byte {byte}"
            );
        }
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        let rendered = [
            BuildError::PageFull,
            BuildError::PacketTooLarge,
            BuildError::TagsTooLong,
        ]
        .map(|error| alloc::format!("{error}"));

        assert_eq!(
            rendered,
            [
                "an Ogg page has no room left for another packet",
                "a packet is longer than the 65024 bytes an Ogg page's lacing table describes",
                "OpusTags content does not fit a 436-byte packet",
            ]
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn build_error_is_a_standard_error() {
        let error: &dyn std::error::Error = &BuildError::PageFull;

        assert_eq!(
            std::string::ToString::to_string(error),
            "an Ogg page has no room left for another packet"
        );
    }
}
