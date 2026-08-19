//! One Ogg page, read out of the buffer it already sits in.

use core::fmt;

use super::crc::{crc32, crc32_from};
use super::{
    CHECKSUM_AT, CHECKSUM_LEN, CONTINUES, FLAG_CONTINUED, FLAG_FIRST, FLAG_LAST, HEADER_LEN, MAGIC,
    VERSION,
};

/// Why an Ogg page could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PageError {
    /// Fewer bytes were handed in than a page header occupies.
    TooShort,
    /// The bytes handed in do not start with the capture pattern.
    BadMagic,
    /// The page states a version of the framing this crate does not read.
    BadVersion,
    /// The page's checksum does not match the bytes it covers.
    BadCrc,
    /// The lacing table, or the segments it describes, reach past the bytes handed in.
    TruncatedBody,
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::TooShort => write!(f, "an Ogg page header needs {HEADER_LEN} bytes"),
            Self::BadMagic => f.write_str("these bytes do not start an Ogg page"),
            Self::BadVersion => write!(f, "an Ogg page states a version other than {VERSION}"),
            Self::BadCrc => f.write_str("an Ogg page's checksum does not match its bytes"),
            Self::TruncatedBody => {
                f.write_str("an Ogg page reaches past the end of the bytes handed in")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PageError {}

/// An Ogg page that has been checked over and is ready to be read from.
///
/// The view borrows the page it was parsed from: [`PageView::packets`] slices packets straight
/// out of it as they are iterated. Nothing is copied, so a page can be read out of a
/// memory-mapped file or a stack buffer.
#[derive(Debug, Clone, Copy)]
pub struct PageView<'a> {
    granule_position: u64,
    serial: u32,
    sequence: u32,
    flags: u8,
    lacing: &'a [u8],
    body: &'a [u8],
}

impl<'a> PageView<'a> {
    /// Reads the Ogg page at the front of `page`.
    ///
    /// Only that one page is read. Whatever follows it is left alone, so walking a file means
    /// handing in everything from a page's offset onwards and stepping on by
    /// [`total_len`](PageView::total_len). The page itself has to be there in full: a slice that
    /// ends inside it is an error, not a partial page.
    ///
    /// The checksum is verified here, so a view that exists is a page whose bytes add up.
    ///
    /// The continued-packet flag (0x01) is not interpreted: a page that states it starts with the
    /// tail of a packet the page before it began, and [`packets`](PageView::packets) hands that
    /// tail out as if it were a packet of its own. Joining it to its head is the caller's
    /// business — [`is_continued`](PageView::is_continued) is how a caller learns it has one to do
    /// — and a TAF never asks for it: teddycloud ends every page on a packet boundary and sets the
    /// flag nowhere.
    ///
    /// # Errors
    ///
    /// - [`PageError::TooShort`] if fewer bytes were handed in than the 27 a page header
    ///   occupies.
    /// - [`PageError::BadMagic`] if the page does not start with `OggS`.
    /// - [`PageError::BadVersion`] if the page states a version other than 0.
    /// - [`PageError::TruncatedBody`] if the lacing table, or the segments it describes, reach
    ///   past the end of the slice.
    /// - [`PageError::BadCrc`] if the page's checksum does not match the bytes it covers.
    ///
    /// # Examples
    ///
    /// ```
    /// use taf::ogg::PageView;
    ///
    /// # let file = include_bytes!("../../tests/fixtures/golden-sine.taf");
    /// // A TAF's audio region starts at offset 4096, with the page carrying `OpusHead`.
    /// let view = PageView::parse(&file[4096..])?;
    ///
    /// assert!(view.is_first());
    /// assert_eq!(view.total_len(), 47);
    /// assert_eq!(view.packets().next().map(|packet| packet.len()), Some(19));
    ///
    /// // Stepping on by what a page occupies lands on the next one.
    /// let next = PageView::parse(&file[4096 + view.total_len()..])?;
    /// assert_eq!(next.sequence(), 1);
    /// # Ok::<(), taf::ogg::PageError>(())
    /// ```
    pub fn parse(page: &'a [u8]) -> Result<Self, PageError> {
        let header: &[u8; HEADER_LEN] = page.first_chunk().ok_or(PageError::TooShort)?;

        if !header.starts_with(MAGIC) {
            return Err(PageError::BadMagic);
        }

        // What RFC 3533 lays out behind the capture pattern: the version and the type flags, then
        // the granule position, the serial number, the sequence number and the checksum, all
        // little-endian, and last how many lacing values follow.
        let &[_, _, _, _, version, flags, fields @ ..] = header;
        let [granule @ .., s0, s1, s2, s3, q0, q1, q2, q3, k0, k1, k2, k3, segment_count] = fields;

        if version != VERSION {
            return Err(PageError::BadVersion);
        }

        let lacing_end = HEADER_LEN + usize::from(segment_count);
        let lacing = page
            .get(HEADER_LEN..lacing_end)
            .ok_or(PageError::TruncatedBody)?;
        // Every lacing value states a segment's length, and 255 of them state at most 65 025
        // bytes, so this sum stays far inside a `usize`.
        let body_len: usize = lacing.iter().map(|&value| usize::from(value)).sum();
        let body = page
            .get(lacing_end..lacing_end + body_len)
            .ok_or(PageError::TruncatedBody)?;

        if checksum(header, lacing, body) != u32::from_le_bytes([k0, k1, k2, k3]) {
            return Err(PageError::BadCrc);
        }

        Ok(Self {
            granule_position: u64::from_le_bytes(granule),
            serial: u32::from_le_bytes([s0, s1, s2, s3]),
            sequence: u32::from_le_bytes([q0, q1, q2, q3]),
            flags,
            lacing,
            body,
        })
    }

    /// Returns the granule position the page states: the sample the audio it carries plays up to.
    #[must_use]
    pub const fn granule_position(&self) -> u64 {
        self.granule_position
    }

    /// Returns the serial number of the stream the page belongs to.
    ///
    /// Every page of a TAF states the file's audio id here, so
    /// [`AudioId::get`](crate::id::AudioId::get) hands over the number a parsed header states, to
    /// compare against.
    #[must_use]
    pub const fn serial(&self) -> u32 {
        self.serial
    }

    /// Returns where the page sits in its stream, counted from zero.
    #[must_use]
    pub const fn sequence(&self) -> u32 {
        self.sequence
    }

    /// Returns whether the page's first packet finishes one the page before it began — the
    /// continued-packet flag.
    ///
    /// Nothing here joins the two halves: [`packets`](PageView::packets) hands the fragment out as
    /// if it were a packet of its own either way. What this answers is whether it is one. No page
    /// of a TAF states the flag, so a reader of TAF files reads this to reject a file rather than
    /// to piece one together.
    #[must_use]
    pub const fn is_continued(&self) -> bool {
        self.flags & FLAG_CONTINUED != 0
    }

    /// Returns whether the page is the first of its stream — the BOS flag.
    #[must_use]
    pub const fn is_first(&self) -> bool {
        self.flags & FLAG_FIRST != 0
    }

    /// Returns whether the page is the last of its stream — the EOS flag.
    #[must_use]
    pub const fn is_last(&self) -> bool {
        self.flags & FLAG_LAST != 0
    }

    /// Returns how many bytes the page occupies: its header, its lacing table, and its segments.
    ///
    /// This is what a walk over a file advances by, and for every page of a TAF from file offset
    /// 8192 on it is exactly [`PAGE_LEN`](super::PAGE_LEN).
    #[must_use]
    pub const fn total_len(&self) -> usize {
        HEADER_LEN + self.lacing.len() + self.body.len()
    }

    /// Returns the packets the page carries, sliced out of it as they are iterated.
    ///
    /// On a page that states the continued-packet flag (0x01) the first of them is the tail of a
    /// packet the page before it began, handed out as if it were whole: nothing here interprets
    /// that flag.
    #[must_use]
    pub const fn packets(&self) -> Packets<'a> {
        Packets {
            lacing: self.lacing,
            body: self.body,
        }
    }
}

/// The packets a page carries, sliced out of it as they are iterated.
///
/// A packet is spread over as many segments as it needs: a lacing value of 255 says its packet
/// carries on into the next segment, and the first value below 255 ends it — so a packet of
/// exactly 255 bytes is laced as `255, 0`, and a lacing value of 0 on its own is a packet of no
/// bytes at all.
///
/// A page whose *last* lacing value is 255 ends on a packet it does not finish: RFC 3533 has that
/// packet carried on by the next page, which marks itself with the continued-packet flag (0x01).
/// Only packets the page itself completes are yielded here, so iteration ends at such a run rather
/// than handing out the piece of one the page began.
///
/// The other end of that packet is not interpreted at all: on a page that states the
/// continued-packet flag, the first packet yielded here is the fragment that finishes what the
/// page before it began, handed out as if it were whole. A TAF has neither end — teddycloud pads
/// the last packet of every page so the page ends on a packet boundary, and the flag appears
/// nowhere in a TAF — so joining packets across pages is something reading a TAF never has to do.
#[derive(Debug, Clone)]
pub struct Packets<'a> {
    lacing: &'a [u8],
    body: &'a [u8],
}

impl<'a> Iterator for Packets<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let mut len = 0;

        loop {
            // A lacing table that runs out mid-packet leaves nothing whole to hand out, and the
            // values it did state are spent — so this ends the iteration for good.
            let (&value, rest) = self.lacing.split_first()?;
            self.lacing = rest;
            len += usize::from(value);

            if value < CONTINUES {
                // Parsing sized the body from these same lacing values, so it holds them.
                let (packet, rest) = self.body.split_at_checked(len)?;
                self.body = rest;

                return Some(packet);
            }
        }
    }
}

/// The checksum a page's bytes add up to, with the four the checksum itself occupies read as
/// zero, which is how RFC 3533 defines it.
///
/// A page being read stays where it lies, so the sum steps around those four bytes rather than
/// copying the page somewhere they can be blanked.
fn checksum(header: &[u8; HEADER_LEN], lacing: &[u8], body: &[u8]) -> u32 {
    // Both of these are inside a header that is 27 bytes long by its type, so neither is missing.
    let before = header.get(..CHECKSUM_AT).unwrap_or_default();
    let after = header.get(CHECKSUM_AT + CHECKSUM_LEN..).unwrap_or_default();

    let mut sum = crc32(before);
    sum = crc32_from(sum, &[0; CHECKSUM_LEN]);
    sum = crc32_from(sum, after);
    sum = crc32_from(sum, lacing);

    crc32_from(sum, body)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::super::PAGE_LEN;
    use super::*;
    use crate::header::{HeaderView, BLOCK_LEN};
    use alloc::vec;
    use alloc::vec::Vec;

    const GOLDEN: &[u8] = include_bytes!("../../tests/fixtures/golden-sine.taf");

    /// Where the audio region, and so the first Ogg page, starts.
    const FIRST_PAGE_AT: usize = BLOCK_LEN;

    /// The bytes the `OpusHead` page occupies: header, one lacing value, and the 19-byte packet.
    const FIRST_PAGE_LEN: usize = 47;

    /// Where the first audio page starts: behind the two Opus header pages.
    const AUDIO_PAGE_AT: usize = 4608;

    /// Where the first block-aligned page starts.
    const ALIGNED_PAGE_AT: usize = 8192;

    /// The samples per channel one Opus frame carries, which every page's granule advances by a
    /// multiple of.
    const SAMPLES_PER_FRAME: u64 = 2880;

    /// Builds a page around the lacing values and body given, checksummed the way a writer does.
    ///
    /// The checksum comes from the same `crc32` the parser checks against, which the golden
    /// fixture's own stored checksums pin to the format in `crc.rs` — so building pages with it
    /// tests the parser rather than itself.
    fn page(flags: u8, lacing: &[u8], body: &[u8]) -> Vec<u8> {
        let mut page = Vec::new();
        page.extend_from_slice(MAGIC);
        page.push(VERSION);
        page.push(flags);
        page.extend_from_slice(&7_u64.to_le_bytes());
        page.extend_from_slice(&444_913_029_u32.to_le_bytes());
        page.extend_from_slice(&3_u32.to_le_bytes());
        page.extend_from_slice(&[0; CHECKSUM_LEN]);
        page.push(u8::try_from(lacing.len()).unwrap());
        page.extend_from_slice(lacing);
        page.extend_from_slice(body);

        let checksum = crc32(&page);
        page[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_LEN].copy_from_slice(&checksum.to_le_bytes());

        page
    }

    /// A page of `lacing.len()` segments whose body is as long as the lacing values add up to.
    fn laced(lacing: &[u8]) -> Vec<u8> {
        let body_len = lacing.iter().map(|&value| usize::from(value)).sum();

        page(0, lacing, &vec![0xa5; body_len])
    }

    /// The packets a page carries, and one more than a page can possibly hold.
    ///
    /// A page has at most 255 lacing values and so at most 255 packets. Stopping a step past that
    /// is what makes an iterator that never ends fail the test that collects it rather than hang
    /// it.
    fn packets<'a>(view: &PageView<'a>) -> Vec<&'a [u8]> {
        view.packets().take(256).collect()
    }

    fn packet_lens(view: &PageView<'_>) -> Vec<usize> {
        packets(view).iter().map(|packet| packet.len()).collect()
    }

    fn parse_err(page: &[u8]) -> PageError {
        PageView::parse(page).unwrap_err()
    }

    #[test]
    fn parses_the_opus_head_page_the_audio_region_starts_with() {
        let view = PageView::parse(&GOLDEN[FIRST_PAGE_AT..]).unwrap();

        assert!(view.is_first(), "the first page carries the BOS flag");
        assert!(!view.is_last());
        assert_eq!(view.sequence(), 0);
        assert_eq!(view.granule_position(), 0);
        assert_eq!(view.serial(), 444_913_029);
        assert_eq!(view.total_len(), FIRST_PAGE_LEN);

        let packets = packets(&view);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), 19);
        assert!(packets[0].starts_with(b"OpusHead"));
    }

    #[test]
    fn parses_a_block_aligned_audio_page() {
        let view = PageView::parse(&GOLDEN[ALIGNED_PAGE_AT..]).unwrap();
        let header = HeaderView::parse(&GOLDEN[..BLOCK_LEN]).unwrap();

        assert_eq!(view.total_len(), PAGE_LEN);
        assert_eq!(PAGE_LEN, 4096);
        assert_eq!(view.serial(), header.audio_id().get());
        assert_eq!(view.sequence(), 3);
        assert_eq!(view.granule_position(), 31_680);
        assert!(!view.is_first());
        assert!(!view.is_last());
    }

    #[test]
    fn reads_the_page_at_the_front_and_leaves_what_follows_it_alone() {
        let from_the_rest_of_the_file = PageView::parse(&GOLDEN[FIRST_PAGE_AT..]).unwrap();
        let exactly_one_page =
            PageView::parse(&GOLDEN[FIRST_PAGE_AT..FIRST_PAGE_AT + FIRST_PAGE_LEN]).unwrap();

        assert_eq!(from_the_rest_of_the_file.total_len(), FIRST_PAGE_LEN);
        assert_eq!(
            packet_lens(&from_the_rest_of_the_file),
            packet_lens(&exactly_one_page)
        );

        // Stepping on by what the page occupies lands on the next one.
        let next = PageView::parse(&GOLDEN[FIRST_PAGE_AT + FIRST_PAGE_LEN..]).unwrap();
        assert_eq!(next.sequence(), 1);
    }

    #[test]
    fn walks_every_page_of_the_golden_file() {
        let header = HeaderView::parse(&GOLDEN[..BLOCK_LEN]).unwrap();
        let mut at = FIRST_PAGE_AT;
        let mut granule = 0;
        let mut pages = 0;

        while at < GOLDEN.len() {
            let view = PageView::parse(&GOLDEN[at..]).unwrap();

            assert_eq!(view.serial(), header.audio_id().get(), "page at {at}");
            assert_eq!(view.sequence(), pages, "page at {at}");
            assert_eq!(view.is_first(), at == FIRST_PAGE_AT, "page at {at}");
            assert!(!view.is_last(), "page at {at}");
            assert_eq!(
                (view.granule_position() - granule) % SAMPLES_PER_FRAME,
                0,
                "page at {at}"
            );
            if at >= ALIGNED_PAGE_AT {
                assert_eq!(at % PAGE_LEN, 0, "page at {at}");
                assert_eq!(view.total_len(), PAGE_LEN, "page at {at}");
            }

            granule = view.granule_position();
            at += view.total_len();
            pages += 1;
        }

        assert_eq!(pages, 29);
        assert_eq!(
            at,
            GOLDEN.len(),
            "the last page ends at the end of the file"
        );
    }

    #[test]
    fn splits_a_page_into_the_packets_its_lacing_table_describes() {
        let view = PageView::parse(&GOLDEN[AUDIO_PAGE_AT..]).unwrap();

        // Fifteen lacing values for five packets: every packet but the last spans four segments.
        assert_eq!(packet_lens(&view), [895, 716, 743, 738, 450]);
        assert_eq!(view.total_len(), 3584);
        assert_eq!(
            view.total_len(),
            HEADER_LEN + 15 + packet_lens(&view).iter().sum::<usize>()
        );
    }

    #[test]
    fn ends_a_packet_at_the_first_lacing_value_below_255() {
        let single = laced(&[4, 3]);
        let spanning = laced(&[CONTINUES, CONTINUES, 4]);

        assert_eq!(packet_lens(&PageView::parse(&single).unwrap()), [4, 3]);
        assert_eq!(packet_lens(&PageView::parse(&spanning).unwrap()), [514]);
    }

    #[test]
    fn reads_a_lacing_value_of_zero_as_a_packet_of_no_bytes() {
        let empty = laced(&[0]);
        let two_empty = laced(&[0, 0]);
        // A packet of exactly 255 bytes needs the zero to say that it ends there.
        let exactly_full = laced(&[CONTINUES, 0]);

        assert_eq!(packet_lens(&PageView::parse(&empty).unwrap()), [0]);
        assert_eq!(packet_lens(&PageView::parse(&two_empty).unwrap()), [0, 0]);
        assert_eq!(packet_lens(&PageView::parse(&exactly_full).unwrap()), [255]);
    }

    #[test]
    fn ends_at_a_packet_the_page_does_not_finish() {
        // A page whose lacing table ends on 255 carries a packet the next page continues, so
        // there is no whole packet to hand out for it.
        let only_a_fragment = laced(&[CONTINUES, CONTINUES]);
        let one_packet_then_a_fragment = laced(&[4, CONTINUES]);

        assert_eq!(
            packet_lens(&PageView::parse(&only_a_fragment).unwrap()),
            [] as [usize; 0]
        );
        assert_eq!(
            packet_lens(&PageView::parse(&one_packet_then_a_fragment).unwrap()),
            [4]
        );
    }

    #[test]
    fn hands_out_the_fragment_a_continued_page_starts_with_as_a_packet() {
        // The other end of that run, which nothing here interprets: a page that states the
        // continued-packet flag parses like any other, and the tail of the packet the page before
        // it began — the leading four bytes here — is handed out as if it were whole. Joining the
        // two halves stays the caller's business; the flag itself is all this reports.
        let continued = page(FLAG_CONTINUED, &[4, 3], &[0xa5; 7]);
        let view = PageView::parse(&continued).unwrap();

        assert_eq!(packet_lens(&view), [4, 3]);
        assert!(view.is_continued());
        assert!(!view.is_first());
        assert!(!view.is_last());
    }

    #[test]
    fn reads_the_flag_a_page_carries_a_packet_on_from_the_page_before_it_with() {
        // The flag on its own, and beside the two flags that share the byte with it — a page that
        // begins or ends a stream is not one that continues a packet.
        let carries_on = page(FLAG_CONTINUED, &[1], &[0]);
        let begins = page(FLAG_FIRST, &[1], &[0]);
        let ends = page(FLAG_LAST, &[1], &[0]);
        let both = page(FLAG_CONTINUED | FLAG_LAST, &[1], &[0]);

        assert!(PageView::parse(&carries_on).unwrap().is_continued());
        assert!(!PageView::parse(&begins).unwrap().is_continued());
        assert!(!PageView::parse(&ends).unwrap().is_continued());
        assert!(PageView::parse(&both).unwrap().is_continued());

        // And no page of a TAF states it: not the one that opens the stream, and not an aligned
        // audio page either.
        assert!(!PageView::parse(&GOLDEN[FIRST_PAGE_AT..])
            .unwrap()
            .is_continued());
        assert!(!PageView::parse(&GOLDEN[ALIGNED_PAGE_AT..])
            .unwrap()
            .is_continued());
    }

    #[test]
    fn reads_a_page_that_carries_no_segments_at_all() {
        let empty = laced(&[]);
        let view = PageView::parse(&empty).unwrap();

        assert_eq!(view.total_len(), HEADER_LEN);
        assert_eq!(packet_lens(&view), [] as [usize; 0]);
    }

    #[test]
    fn reads_the_flags_a_page_ends_a_stream_with() {
        let ends_the_stream = page(FLAG_LAST, &[1], &[0]);
        let is_the_whole_stream = page(FLAG_FIRST | FLAG_LAST, &[1], &[0]);
        let last = PageView::parse(&ends_the_stream).unwrap();
        let only_page = PageView::parse(&is_the_whole_stream).unwrap();

        assert!(!last.is_first());
        assert!(last.is_last());
        assert!(only_page.is_first());
        assert!(only_page.is_last());
    }

    #[test]
    fn rejects_fewer_bytes_than_a_page_header() {
        assert_eq!(parse_err(&[]), PageError::TooShort);
        assert_eq!(
            parse_err(&GOLDEN[FIRST_PAGE_AT..FIRST_PAGE_AT + HEADER_LEN - 1]),
            PageError::TooShort
        );
    }

    #[test]
    fn rejects_bytes_that_do_not_start_with_the_capture_pattern() {
        let mut broken = GOLDEN[FIRST_PAGE_AT..FIRST_PAGE_AT + FIRST_PAGE_LEN].to_vec();
        broken[3] = b'X';

        // The capture pattern is looked at before the checksum that no longer matches either.
        assert_eq!(parse_err(&broken), PageError::BadMagic);
        assert_eq!(parse_err(&[0; HEADER_LEN]), PageError::BadMagic);
    }

    #[test]
    fn rejects_a_version_of_the_framing_it_does_not_read() {
        let mut future = GOLDEN[FIRST_PAGE_AT..FIRST_PAGE_AT + FIRST_PAGE_LEN].to_vec();
        future[4] = 1;

        assert_eq!(parse_err(&future), PageError::BadVersion);
    }

    #[test]
    fn rejects_a_page_that_reaches_past_the_bytes_handed_in() {
        // A lacing table the slice ends inside of, and a body the slice ends inside of.
        let aligned = &GOLDEN[ALIGNED_PAGE_AT..];

        assert_eq!(
            parse_err(&aligned[..HEADER_LEN + 16]),
            PageError::TruncatedBody
        );
        assert_eq!(
            parse_err(&aligned[..PAGE_LEN - 1]),
            PageError::TruncatedBody
        );
    }

    #[test]
    fn rejects_a_page_whose_checksum_does_not_match_its_bytes() {
        let intact = &GOLDEN[FIRST_PAGE_AT..FIRST_PAGE_AT + FIRST_PAGE_LEN];

        for flipped in [6, 14, HEADER_LEN, FIRST_PAGE_LEN - 1] {
            let mut broken = intact.to_vec();
            broken[flipped] ^= 0x01;

            assert_eq!(parse_err(&broken), PageError::BadCrc, "byte {flipped}");
        }

        // A page that states a checksum of its own that its bytes do not add up to.
        let mut restated = intact.to_vec();
        restated[CHECKSUM_AT] ^= 0x01;
        assert_eq!(parse_err(&restated), PageError::BadCrc);
    }

    #[test]
    fn packets_replays_the_same_packets_from_a_clone() {
        let view = PageView::parse(&GOLDEN[AUDIO_PAGE_AT..]).unwrap();
        let packets = view.packets();
        let replay = packets.clone();

        assert_eq!(packets.take(256).count(), 5);
        assert_eq!(
            replay.take(256).map(<[u8]>::len).collect::<Vec<_>>(),
            [895, 716, 743, 738, 450]
        );
    }

    #[test]
    fn packets_ends_rather_than_panicking_on_a_body_shorter_than_its_lacing_table() {
        // Parsing sizes the body from the lacing table, so this state is unreachable through
        // `parse`.
        let mut packets = Packets {
            lacing: &[3],
            body: &[],
        };

        assert_eq!(packets.next(), None);
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        let rendered = [
            PageError::TooShort,
            PageError::BadMagic,
            PageError::BadVersion,
            PageError::BadCrc,
            PageError::TruncatedBody,
        ]
        .map(|error| alloc::format!("{error}"));

        assert_eq!(
            rendered,
            [
                "an Ogg page header needs 27 bytes",
                "these bytes do not start an Ogg page",
                "an Ogg page states a version other than 0",
                "an Ogg page's checksum does not match its bytes",
                "an Ogg page reaches past the end of the bytes handed in",
            ]
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn page_error_is_a_standard_error() {
        let error: &dyn std::error::Error = &PageError::BadCrc;

        assert_eq!(
            std::string::ToString::to_string(error),
            "an Ogg page's checksum does not match its bytes"
        );
    }
}
