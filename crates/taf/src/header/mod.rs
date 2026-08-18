//! The 4096-byte header block that starts every TAF file, read without copying.
//!
//! A header block is a four-byte big-endian length prefix, a protobuf message of that length,
//! and zero padding out to the end of the block. [`HeaderView::parse`] checks the framing once
//! and then borrows the block: the hash stays where it is and the chapter list is decoded while
//! it is iterated. `FORMAT.md` in this crate describes the wire format and is authoritative.

mod varint;

use core::fmt;

use crate::id::{AudioId, BlockIndex};

/// The length of a TAF header block, and the granularity of everything that follows it.
///
/// The header occupies exactly one block, and so does every block of the audio region.
pub const BLOCK_LEN: usize = 4096;

/// The bytes the big-endian length prefix occupies at the front of the block.
const PREFIX_LEN: usize = 4;

/// The longest protobuf message a block can hold: the block without its length prefix.
const MAX_MESSAGE_LEN: usize = BLOCK_LEN - PREFIX_LEN;

/// The length of the SHA-1 the header carries.
const SHA1_LEN: usize = 20;

/// `sha1_hash`: the SHA-1 of the audio region.
const FIELD_SHA1: u32 = 1;
/// `num_bytes`: the length of the audio region.
const FIELD_NUM_BYTES: u32 = 2;
/// `audio_id`: the content id, which is also every Ogg page's serial number.
const FIELD_AUDIO_ID: u32 = 3;
/// `track_page_nums`: the packed chapter starts.
const FIELD_TRACK_PAGE_NUMS: u32 = 4;
/// `_fill`: the zero bytes that pad the message out to the block.
const FIELD_FILL: u32 = 5;

/// Wire type 0: a varint.
const WIRE_VARINT: u32 = 0;
/// Wire type 1: eight fixed bytes.
const WIRE_FIXED64: u32 = 1;
/// Wire type 2: a length prefix followed by that many bytes.
const WIRE_LEN_DELIMITED: u32 = 2;
/// Wire type 5: four fixed bytes.
const WIRE_FIXED32: u32 = 5;

/// The bits of a field key that hold its wire type; the rest hold the field number.
const WIRE_TYPE_BITS: u32 = 3;

/// Why a header block could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeaderError {
    /// The block handed in was not exactly [`BLOCK_LEN`] bytes long.
    WrongBlockLen,
    /// The length prefix claimed a message that does not fit the block.
    PrefixOutOfRange,
    /// A field reached past the end of the message, or the hash was not 20 bytes long.
    TruncatedField,
    /// A field the format requires was absent; carries its field number.
    MissingField(u32),
    /// A field appeared more than once; carries its field number.
    DuplicateField(u32),
    /// A field carried a wire type this parser cannot read; carries its field number.
    UnexpectedWireType(u32),
    /// A varint carried more bits than the field it belongs to holds.
    VarintOverflow,
    /// A field held a value too large for the type this crate reads it into; carries its field
    /// number.
    FieldOutOfRange(u32),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::WrongBlockLen => write!(f, "TAF header block is not {BLOCK_LEN} bytes long"),
            Self::PrefixOutOfRange => {
                write!(
                    f,
                    "TAF header message is longer than {MAX_MESSAGE_LEN} bytes"
                )
            }
            Self::TruncatedField => {
                f.write_str("a TAF header field reaches past the end of the header message")
            }
            Self::MissingField(field) => write!(f, "TAF header field {field} is missing"),
            Self::DuplicateField(field) => {
                write!(f, "TAF header field {field} appears more than once")
            }
            Self::UnexpectedWireType(field) => {
                write!(f, "TAF header field {field} has an unreadable wire type")
            }
            Self::VarintOverflow => f.write_str("a TAF header varint is wider than its field"),
            Self::FieldOutOfRange(field) => {
                write!(f, "TAF header field {field} holds an out-of-range value")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for HeaderError {}

/// A TAF header block that has been checked over and is ready to be read from.
///
/// The view borrows the block it was parsed from: [`HeaderView::sha1`] points into it and
/// [`HeaderView::chapter_pages`] decodes the chapter list from it on demand. Nothing is copied,
/// so a header can be read straight out of a memory-mapped file or a stack buffer.
///
/// Fields the parser does not know are skipped by wire type, which is what lets it read the
/// bookkeeping fields teddycloud appends *after* the fill field, and any field a later format
/// version adds.
#[derive(Debug, Clone, Copy)]
pub struct HeaderView<'a> {
    sha1: &'a [u8; SHA1_LEN],
    data_length: u32,
    audio_id: AudioId,
    chapters: &'a [u8],
    chapter_count: usize,
}

impl<'a> HeaderView<'a> {
    /// Reads the header block at the start of a TAF file.
    ///
    /// `block` must be the whole block, all [`BLOCK_LEN`] bytes of it; whatever follows the
    /// message inside the block is ignored, which covers both the usual zero padding and the
    /// one trailing byte a 4091-byte message leaves.
    ///
    /// # Errors
    ///
    /// - [`HeaderError::WrongBlockLen`] if `block` is not exactly [`BLOCK_LEN`] bytes.
    /// - [`HeaderError::PrefixOutOfRange`] if the length prefix exceeds what the block holds.
    /// - [`HeaderError::TruncatedField`] if a field reaches past the end of the message, or the
    ///   hash is not 20 bytes long.
    /// - [`HeaderError::MissingField`] if the hash, the audio length, the audio id or the fill
    ///   is absent. An absent chapter list is read as no chapters rather than an error.
    /// - [`HeaderError::DuplicateField`] if any of those fields appears twice, including the
    ///   chapter list, which this crate reads as a single packed run.
    /// - [`HeaderError::UnexpectedWireType`] if a field carries a wire type that does not match
    ///   the format — including a chapter list written unpacked — or one that cannot be skipped,
    ///   such as the deprecated group types.
    /// - [`HeaderError::VarintOverflow`] if a varint carries more bits than its field holds.
    /// - [`HeaderError::FieldOutOfRange`] if the audio length does not fit a `u32`. It is a
    ///   `uint64` on the wire, but the format bounds it at 2 147 479 551 bytes.
    pub fn parse(block: &'a [u8]) -> Result<Self, HeaderError> {
        let block: &'a [u8; BLOCK_LEN] =
            block.try_into().map_err(|_| HeaderError::WrongBlockLen)?;
        // The block's length is a type-level fact by now, so this split cannot fail.
        let &[p0, p1, p2, p3, ref padded_message @ ..] = block;

        // A prefix too wide for a `usize` cannot fit the block either, so folding it into the
        // largest `usize` leaves the bounds check below to reject it.
        let message_len =
            usize::try_from(u32::from_be_bytes([p0, p1, p2, p3])).unwrap_or(usize::MAX);
        let message = padded_message
            .get(..message_len)
            .ok_or(HeaderError::PrefixOutOfRange)?;

        Self::read_fields(message)
    }

    /// Walks the protobuf message, collecting the fields this crate reads.
    fn read_fields(message: &'a [u8]) -> Result<Self, HeaderError> {
        let mut sha1: Option<&'a [u8; SHA1_LEN]> = None;
        let mut data_length: Option<u32> = None;
        let mut audio_id: Option<u32> = None;
        let mut chapters: Option<&'a [u8]> = None;
        let mut fill: Option<()> = None;
        let mut rest = message;

        while !rest.is_empty() {
            let (key, consumed) = varint::decode_u32(rest)?;
            rest = advance(rest, consumed);
            let field = key >> WIRE_TYPE_BITS;
            let wire = key & ((1 << WIRE_TYPE_BITS) - 1);

            rest = match field {
                FIELD_SHA1 => {
                    expect_wire(field, wire, WIRE_LEN_DELIMITED)?;
                    let (payload, tail) = read_len_delimited(rest)?;
                    let hash = payload
                        .try_into()
                        .map_err(|_| HeaderError::TruncatedField)?;
                    set_once(&mut sha1, hash, field)?;
                    tail
                }
                FIELD_NUM_BYTES => {
                    expect_wire(field, wire, WIRE_VARINT)?;
                    let (value, consumed) = varint::decode_u64(rest)?;
                    let value =
                        u32::try_from(value).map_err(|_| HeaderError::FieldOutOfRange(field))?;
                    set_once(&mut data_length, value, field)?;
                    advance(rest, consumed)
                }
                FIELD_AUDIO_ID => {
                    expect_wire(field, wire, WIRE_VARINT)?;
                    let (value, consumed) = varint::decode_u32(rest)?;
                    set_once(&mut audio_id, value, field)?;
                    advance(rest, consumed)
                }
                FIELD_TRACK_PAGE_NUMS => {
                    expect_wire(field, wire, WIRE_LEN_DELIMITED)?;
                    let (payload, tail) = read_len_delimited(rest)?;
                    set_once(&mut chapters, payload, field)?;
                    tail
                }
                FIELD_FILL => {
                    expect_wire(field, wire, WIRE_LEN_DELIMITED)?;
                    let (_, tail) = read_len_delimited(rest)?;
                    set_once(&mut fill, (), field)?;
                    tail
                }
                _ => skip_field(field, wire, rest)?,
            };
        }

        let sha1 = sha1.ok_or(HeaderError::MissingField(FIELD_SHA1))?;
        let data_length = data_length.ok_or(HeaderError::MissingField(FIELD_NUM_BYTES))?;
        let audio_id = audio_id.ok_or(HeaderError::MissingField(FIELD_AUDIO_ID))?;
        fill.ok_or(HeaderError::MissingField(FIELD_FILL))?;

        // A file without a chapter list has no chapters. teddycloud always writes one, but the
        // field is `repeated`, and an empty repeated field is written by writing nothing.
        let chapters = chapters.unwrap_or_default();
        let chapter_count = count_chapters(chapters)?;

        Ok(Self {
            sha1,
            data_length,
            audio_id: AudioId::new(audio_id),
            chapters,
            chapter_count,
        })
    }

    /// Returns the SHA-1 of the audio region, still in the block it was parsed from.
    #[must_use]
    pub const fn sha1(&self) -> &'a [u8; SHA1_LEN] {
        self.sha1
    }

    /// Returns the length of the audio region in bytes, as the header states it.
    ///
    /// A file the box accepts states a multiple of [`BLOCK_LEN`] here, and one that matches the
    /// bytes after the header block; neither is checked while parsing, because a reader that
    /// cannot report what a broken file claims cannot explain why it is broken.
    #[must_use]
    pub const fn data_length(&self) -> u32 {
        self.data_length
    }

    /// Returns the file's audio id, which is also the serial number of its every Ogg page.
    #[must_use]
    pub const fn audio_id(&self) -> AudioId {
        self.audio_id
    }

    /// Returns the blocks the file's chapters start at, decoded as they are iterated.
    #[must_use]
    pub const fn chapter_pages(&self) -> ChapterPages<'a> {
        ChapterPages {
            rest: self.chapters,
        }
    }

    /// Returns how many chapters the file has, counted while the header was parsed.
    #[must_use]
    pub const fn chapter_count(&self) -> usize {
        self.chapter_count
    }
}

/// The chapter starts of a header, decoded one at a time from the block they live in.
///
/// Parsing the header already walked the packed run this iterates, so decoding cannot fail here
/// and there is nothing to report: bytes that somehow do not decode end the iteration.
#[derive(Debug, Clone)]
pub struct ChapterPages<'a> {
    rest: &'a [u8],
}

impl Iterator for ChapterPages<'_> {
    type Item = BlockIndex;

    fn next(&mut self) -> Option<Self::Item> {
        let (value, consumed) = varint::decode_u32(self.rest).ok()?;
        self.rest = advance(self.rest, consumed);

        Some(BlockIndex::new(value))
    }
}

/// Counts the entries of a packed run of chapter starts, rejecting any that does not decode.
///
/// Doing this once while parsing is what lets [`ChapterPages`] be infallible.
fn count_chapters(mut packed: &[u8]) -> Result<usize, HeaderError> {
    let mut count = 0;

    while !packed.is_empty() {
        let (_, consumed) = varint::decode_u32(packed)?;
        packed = advance(packed, consumed);
        count += 1;
    }

    Ok(count)
}

/// Drops the `consumed` bytes a decoder just read from the front of `buf`.
///
/// Every decoder here reads at least one byte and never more than `buf` holds. Holding it to
/// that range anyway is what makes the walks that call this finite and in bounds no matter what
/// a decoder reports — a parser fed a hostile block can fail, but it cannot hang or panic.
fn advance(buf: &[u8], consumed: usize) -> &[u8] {
    buf.get(consumed.max(1)..).unwrap_or_default()
}

/// Checks that a field carries the wire type the format gives it.
fn expect_wire(field: u32, wire: u32, expected: u32) -> Result<(), HeaderError> {
    if wire == expected {
        Ok(())
    } else {
        Err(HeaderError::UnexpectedWireType(field))
    }
}

/// Records a field's value, refusing a second copy of a field that may only appear once.
fn set_once<T>(slot: &mut Option<T>, value: T, field: u32) -> Result<(), HeaderError> {
    match slot.replace(value) {
        None => Ok(()),
        Some(_) => Err(HeaderError::DuplicateField(field)),
    }
}

/// Reads a length-delimited field, returning its payload and the bytes after it.
fn read_len_delimited(buf: &[u8]) -> Result<(&[u8], &[u8]), HeaderError> {
    let (len, consumed) = varint::decode_u32(buf)?;
    // A payload longer than a `usize` cannot be in the message, so the split below rejects it.
    let len = usize::try_from(len).unwrap_or(usize::MAX);

    advance(buf, consumed)
        .split_at_checked(len)
        .ok_or(HeaderError::TruncatedField)
}

/// Steps over a field this parser has no use for, returning the bytes after it.
///
/// Skipping by wire type is what keeps a reader working when a writer adds fields — including
/// the four teddycloud writes for its own bookkeeping. Only the group wire types, deprecated
/// long before this format existed, and the two that mean nothing at all cannot be stepped over.
fn skip_field(field: u32, wire: u32, buf: &[u8]) -> Result<&[u8], HeaderError> {
    match wire {
        WIRE_VARINT => {
            let (_, consumed) = varint::decode_u64(buf)?;
            Ok(advance(buf, consumed))
        }
        WIRE_FIXED64 => split_fixed(buf, 8),
        WIRE_LEN_DELIMITED => read_len_delimited(buf).map(|(_, tail)| tail),
        WIRE_FIXED32 => split_fixed(buf, 4),
        _ => Err(HeaderError::UnexpectedWireType(field)),
    }
}

/// Steps over a fixed-width field, returning the bytes after it.
fn split_fixed(buf: &[u8], width: usize) -> Result<&[u8], HeaderError> {
    buf.split_at_checked(width)
        .map(|(_, tail)| tail)
        .ok_or(HeaderError::TruncatedField)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const GOLDEN: &[u8] = include_bytes!("../../tests/fixtures/golden-sine.taf");
    const REAL_1: &[u8] = include_bytes!("../../tests/fixtures/real-header-1.bin");
    const REAL_2: &[u8] = include_bytes!("../../tests/fixtures/real-header-2.bin");

    const SHA1: [u8; SHA1_LEN] = [0x11; SHA1_LEN];

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn key(field: u32, wire: u32) -> Vec<u8> {
        varint(u64::from(field << 3 | wire))
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = key(field, WIRE_VARINT);
        out.extend(varint(value));
        out
    }

    fn bytes_field(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = key(field, WIRE_LEN_DELIMITED);
        out.extend(varint(u64::try_from(payload.len()).unwrap()));
        out.extend_from_slice(payload);
        out
    }

    /// The required fields plus a one-chapter list, in the order teddycloud writes them.
    fn required_message() -> Vec<u8> {
        message_with_chapters(&[0])
    }

    /// The required fields around a chapter list given as raw packed bytes.
    fn message_with_chapters(packed: &[u8]) -> Vec<u8> {
        let mut message = bytes_field(FIELD_SHA1, &SHA1);
        message.extend(varint_field(FIELD_NUM_BYTES, 110_592));
        message.extend(varint_field(FIELD_AUDIO_ID, 444_913_029));
        message.extend(bytes_field(FIELD_TRACK_PAGE_NUMS, packed));
        message.extend(bytes_field(FIELD_FILL, &[]));
        message
    }

    /// The required fields with the fill sized so the message is exactly `total` bytes.
    fn message_of_len(total: usize) -> Vec<u8> {
        let mut message = bytes_field(FIELD_SHA1, &SHA1);
        message.extend(varint_field(FIELD_NUM_BYTES, 110_592));
        message.extend(varint_field(FIELD_AUDIO_ID, 444_913_029));
        message.extend(bytes_field(FIELD_TRACK_PAGE_NUMS, &[0]));
        // A tag byte plus the two-byte length varint of a fill this long.
        let fill = total - message.len() - 3;
        message.extend(bytes_field(FIELD_FILL, &vec![0; fill]));
        assert_eq!(message.len(), total);
        message
    }

    fn block(message: &[u8]) -> [u8; BLOCK_LEN] {
        block_with_prefix(u32::try_from(message.len()).unwrap(), message)
    }

    fn block_with_prefix(prefix: u32, message: &[u8]) -> [u8; BLOCK_LEN] {
        let mut block = [0_u8; BLOCK_LEN];
        block[..PREFIX_LEN].copy_from_slice(&prefix.to_be_bytes());
        block[PREFIX_LEN..PREFIX_LEN + message.len()].copy_from_slice(message);
        block
    }

    fn chapters(view: &HeaderView<'_>) -> Vec<u32> {
        view.chapter_pages().map(BlockIndex::get).collect()
    }

    fn parse_err(block: &[u8]) -> HeaderError {
        HeaderView::parse(block).unwrap_err()
    }

    #[test]
    fn parses_the_golden_fixture_header() {
        let view = HeaderView::parse(&GOLDEN[..BLOCK_LEN]).unwrap();

        assert_eq!(
            view.sha1(),
            &[
                0x1a, 0xc8, 0x22, 0xae, 0x55, 0x0f, 0x04, 0xdc, 0xe1, 0x3e, 0xd2, 0x23, 0xbb, 0xa9,
                0x92, 0x7f, 0xb8, 0x45, 0xa1, 0xf4
            ]
        );
        assert_eq!(view.data_length(), 110_592);
        assert_eq!(view.audio_id(), AudioId::new(444_913_029));
        assert_eq!(view.chapter_count(), 1);
        assert_eq!(chapters(&view), [0]);
        assert_eq!(
            view.chapter_pages().next(),
            Some(BlockIndex::new(0)),
            "every TAF starts a chapter at block 0"
        );
    }

    #[test]
    fn parses_the_three_chapter_header() {
        let view = HeaderView::parse(REAL_1).unwrap();

        assert_eq!(
            view.sha1(),
            &[
                0x6c, 0xdb, 0x1e, 0x91, 0x31, 0x99, 0x93, 0xb8, 0x61, 0xfe, 0x72, 0x1a, 0xb2, 0xaf,
                0xab, 0xb2, 0x5d, 0x68, 0xed, 0x05
            ]
        );
        assert_eq!(view.data_length(), 339_968);
        assert_eq!(view.audio_id(), AudioId::new(444_913_053));
        assert_eq!(view.chapter_count(), 3);
        assert_eq!(chapters(&view), [0, 27, 55]);
    }

    #[test]
    fn parses_the_two_chapter_header() {
        let view = HeaderView::parse(REAL_2).unwrap();

        assert_eq!(
            view.sha1(),
            &[
                0x4a, 0xdb, 0x8a, 0x3f, 0x15, 0x6a, 0x71, 0xa0, 0x7f, 0x5f, 0xe8, 0xa6, 0x39, 0x9a,
                0xa7, 0xf1, 0x21, 0xe3, 0x8e, 0xe9
            ]
        );
        assert_eq!(view.data_length(), 5_443_584);
        assert_eq!(view.audio_id(), AudioId::new(444_913_094));
        assert_eq!(view.chapter_count(), 2);
        assert_eq!(chapters(&view), [0, 663]);
    }

    #[test]
    fn rejects_a_block_that_is_not_exactly_one_block_long() {
        assert_eq!(
            parse_err(&GOLDEN[..BLOCK_LEN - 1]),
            HeaderError::WrongBlockLen
        );
        assert_eq!(parse_err(&GOLDEN[..=BLOCK_LEN]), HeaderError::WrongBlockLen);
    }

    #[test]
    fn rejects_a_zeroed_block_as_a_missing_hash() {
        // A zeroed block claims a zero-length message, so the first thing missing is field 1.
        assert_eq!(
            parse_err(&[0_u8; BLOCK_LEN]),
            HeaderError::MissingField(FIELD_SHA1)
        );
    }

    #[test]
    fn rejects_a_prefix_that_reaches_past_the_block() {
        let message = required_message();

        assert_eq!(
            parse_err(&block_with_prefix(
                u32::try_from(MAX_MESSAGE_LEN).unwrap() + 1,
                &message
            )),
            HeaderError::PrefixOutOfRange
        );
        assert_eq!(
            parse_err(&block_with_prefix(u32::MAX, &message)),
            HeaderError::PrefixOutOfRange
        );
    }

    #[test]
    fn accepts_a_message_that_fills_the_block_exactly() {
        let raw = block(&message_of_len(MAX_MESSAGE_LEN));
        let view = HeaderView::parse(&raw).unwrap();

        assert_eq!(chapters(&view), [0]);
    }

    #[test]
    fn accepts_the_one_byte_shorter_message_teddycloud_can_emit() {
        // teddycloud's fill sizing lands on 4091 bytes when the fill's own length varint
        // shrinks; the block then ends in one zero byte that is not part of the message.
        let raw = block(&message_of_len(MAX_MESSAGE_LEN - 1));
        let view = HeaderView::parse(&raw).unwrap();

        assert_eq!(chapters(&view), [0]);
    }

    #[test]
    fn ignores_whatever_follows_the_message() {
        let message = required_message();
        let mut raw = block(&message);
        raw[PREFIX_LEN + message.len()..].fill(0xff);

        assert_eq!(HeaderView::parse(&raw).unwrap().data_length(), 110_592);
    }

    #[test]
    fn keeps_reading_the_fields_that_follow_the_fill() {
        // teddycloud writes its own bookkeeping fields 6..=9 *after* the fill field.
        let mut message = message_with_chapters(&[0, 27]);
        message.extend(varint_field(6, 478_080));
        message.extend(varint_field(7, 168));
        message.extend(varint_field(8, 27));
        message.extend(varint_field(9, 29));

        let raw = block(&message);
        let view = HeaderView::parse(&raw).unwrap();

        assert_eq!(chapters(&view), [0, 27]);
    }

    #[test]
    fn skips_unknown_fields_of_every_skippable_wire_type() {
        let mut message = key(100, WIRE_FIXED64);
        message.extend_from_slice(&[0xff; 8]);
        message.extend(bytes_field(101, &[0xff; 3]));
        message.extend(key(102, WIRE_FIXED32));
        message.extend_from_slice(&[0xff; 4]);
        message.extend(varint_field(103, u64::MAX));
        message.extend(required_message());

        let raw = block(&message);
        let view = HeaderView::parse(&raw).unwrap();

        assert_eq!(view.audio_id(), AudioId::new(444_913_029));
    }

    #[test]
    fn rejects_a_wire_type_it_cannot_skip() {
        for wire in [3, 4, 6, 7] {
            let mut message = required_message();
            message.extend(key(100, wire));

            assert_eq!(
                parse_err(&block(&message)),
                HeaderError::UnexpectedWireType(100),
                "wire type {wire}"
            );
        }
    }

    #[test]
    fn rejects_a_fixed_width_field_the_message_ends_inside_of() {
        let mut message = required_message();
        message.extend(key(100, WIRE_FIXED64));
        message.extend_from_slice(&[0xff; 7]);

        assert_eq!(parse_err(&block(&message)), HeaderError::TruncatedField);

        let mut message = required_message();
        message.extend(key(101, WIRE_FIXED32));
        message.extend_from_slice(&[0xff; 3]);

        assert_eq!(parse_err(&block(&message)), HeaderError::TruncatedField);
    }

    #[test]
    fn rejects_a_length_delimited_field_that_reaches_past_the_message() {
        let mut message = required_message();
        message.extend(key(100, WIRE_LEN_DELIMITED));
        message.extend(varint(1));

        assert_eq!(parse_err(&block(&message)), HeaderError::TruncatedField);
    }

    #[test]
    fn rejects_a_message_that_ends_in_the_middle_of_a_field() {
        // One tail per place a field can run out of bytes: a key, a length, a payload of each
        // shape, and the varint of a field this parser only steps over.
        let mut hash = key(FIELD_SHA1, WIRE_LEN_DELIMITED);
        hash.extend(varint(u64::try_from(SHA1_LEN).unwrap()));
        hash.extend_from_slice(&SHA1[..SHA1_LEN - 1]);

        let mut chapter_list = key(FIELD_TRACK_PAGE_NUMS, WIRE_LEN_DELIMITED);
        chapter_list.extend(varint(3));
        chapter_list.extend_from_slice(&[0, 27]);

        let mut fill = key(FIELD_FILL, WIRE_LEN_DELIMITED);
        fill.extend(varint(1));

        let mut fill_length = key(FIELD_FILL, WIRE_LEN_DELIMITED);
        fill_length.push(0x80);

        let mut num_bytes = key(FIELD_NUM_BYTES, WIRE_VARINT);
        num_bytes.push(0x80);

        let mut unknown = key(100, WIRE_VARINT);
        unknown.push(0x80);

        let tails = [
            hash,
            chapter_list,
            fill,
            fill_length,
            num_bytes,
            unknown,
            vec![0x80],
        ];

        for tail in tails {
            let mut message = required_message();
            message.extend_from_slice(&tail);

            assert_eq!(
                parse_err(&block(&message)),
                HeaderError::TruncatedField,
                "tail {tail:02x?}"
            );
        }
    }

    #[test]
    fn rejects_known_fields_carrying_the_wrong_wire_type() {
        let wrong = [
            (FIELD_SHA1, WIRE_VARINT),
            (FIELD_NUM_BYTES, WIRE_LEN_DELIMITED),
            (FIELD_AUDIO_ID, WIRE_LEN_DELIMITED),
            (FIELD_TRACK_PAGE_NUMS, WIRE_VARINT),
            (FIELD_FILL, WIRE_VARINT),
        ];

        for (field, wire) in wrong {
            let mut message = key(field, wire);
            message.extend(varint(0));

            assert_eq!(
                parse_err(&block(&message)),
                HeaderError::UnexpectedWireType(field),
                "field {field}"
            );
        }
    }

    #[test]
    fn rejects_a_field_that_appears_twice() {
        let repeats = [
            (FIELD_SHA1, bytes_field(FIELD_SHA1, &SHA1)),
            (FIELD_NUM_BYTES, varint_field(FIELD_NUM_BYTES, 1)),
            (FIELD_AUDIO_ID, varint_field(FIELD_AUDIO_ID, 1)),
            (
                FIELD_TRACK_PAGE_NUMS,
                bytes_field(FIELD_TRACK_PAGE_NUMS, &[1]),
            ),
            (FIELD_FILL, bytes_field(FIELD_FILL, &[])),
        ];

        for (field, repeat) in repeats {
            let mut message = required_message();
            message.extend(repeat);

            assert_eq!(
                parse_err(&block(&message)),
                HeaderError::DuplicateField(field),
                "field {field}"
            );
        }
    }

    #[test]
    fn rejects_a_hash_that_is_not_twenty_bytes() {
        for len in [SHA1_LEN - 1, SHA1_LEN + 1] {
            let mut message = bytes_field(FIELD_SHA1, &vec![0x11; len]);
            message.extend(varint_field(FIELD_NUM_BYTES, 1));
            message.extend(varint_field(FIELD_AUDIO_ID, 1));
            message.extend(bytes_field(FIELD_FILL, &[]));

            assert_eq!(
                parse_err(&block(&message)),
                HeaderError::TruncatedField,
                "hash of {len} bytes"
            );
        }
    }

    #[test]
    fn reports_each_missing_required_field_by_number() {
        let fields = [
            (FIELD_SHA1, bytes_field(FIELD_SHA1, &SHA1)),
            (FIELD_NUM_BYTES, varint_field(FIELD_NUM_BYTES, 1)),
            (FIELD_AUDIO_ID, varint_field(FIELD_AUDIO_ID, 1)),
            (FIELD_FILL, bytes_field(FIELD_FILL, &[])),
        ];

        for (missing, _) in &fields {
            let mut message = Vec::new();
            for (field, bytes) in &fields {
                if field != missing {
                    message.extend_from_slice(bytes);
                }
            }

            assert_eq!(
                parse_err(&block(&message)),
                HeaderError::MissingField(*missing)
            );
        }
    }

    #[test]
    fn reads_a_num_bytes_that_needs_all_of_u32() {
        let mut message = bytes_field(FIELD_SHA1, &SHA1);
        message.extend(varint_field(FIELD_NUM_BYTES, u64::from(u32::MAX)));
        message.extend(varint_field(FIELD_AUDIO_ID, 1));
        message.extend(bytes_field(FIELD_FILL, &[]));

        assert_eq!(
            HeaderView::parse(&block(&message)).unwrap().data_length(),
            u32::MAX
        );
    }

    #[test]
    fn rejects_a_num_bytes_wider_than_u32() {
        // The field is a `uint64` on the wire, so a value this crate cannot hold is a
        // well-formed varint carrying an out-of-range value, not a malformed varint.
        let mut message = bytes_field(FIELD_SHA1, &SHA1);
        message.extend(varint_field(FIELD_NUM_BYTES, u64::from(u32::MAX) + 1));
        message.extend(varint_field(FIELD_AUDIO_ID, 1));
        message.extend(bytes_field(FIELD_FILL, &[]));

        assert_eq!(
            parse_err(&block(&message)),
            HeaderError::FieldOutOfRange(FIELD_NUM_BYTES)
        );
    }

    #[test]
    fn rejects_an_audio_id_wider_than_u32() {
        let mut message = bytes_field(FIELD_SHA1, &SHA1);
        message.extend(varint_field(FIELD_NUM_BYTES, 1));
        message.extend(varint_field(FIELD_AUDIO_ID, u64::from(u32::MAX) + 1));
        message.extend(bytes_field(FIELD_FILL, &[]));

        assert_eq!(parse_err(&block(&message)), HeaderError::VarintOverflow);
    }

    #[test]
    fn treats_an_absent_chapter_list_as_no_chapters() {
        let mut message = bytes_field(FIELD_SHA1, &SHA1);
        message.extend(varint_field(FIELD_NUM_BYTES, 1));
        message.extend(varint_field(FIELD_AUDIO_ID, 1));
        message.extend(bytes_field(FIELD_FILL, &[]));

        let raw = block(&message);
        let view = HeaderView::parse(&raw).unwrap();

        assert_eq!(view.chapter_count(), 0);
        assert_eq!(view.chapter_pages().next(), None);
    }

    #[test]
    fn rejects_a_chapter_list_that_does_not_decode() {
        let truncated = message_with_chapters(&[0x80]);
        assert_eq!(parse_err(&block(&truncated)), HeaderError::TruncatedField);

        let too_wide = message_with_chapters(&[0x80, 0x80, 0x80, 0x80, 0x10]);
        assert_eq!(parse_err(&block(&too_wide)), HeaderError::VarintOverflow);
    }

    #[test]
    fn reads_fields_in_any_order() {
        let mut message = bytes_field(FIELD_FILL, &[0, 0]);
        message.extend(bytes_field(FIELD_TRACK_PAGE_NUMS, &[0, 27]));
        message.extend(varint_field(FIELD_AUDIO_ID, 7));
        message.extend(varint_field(FIELD_NUM_BYTES, 4096));
        message.extend(bytes_field(FIELD_SHA1, &SHA1));

        let raw = block(&message);
        let view = HeaderView::parse(&raw).unwrap();

        assert_eq!(view.sha1(), &SHA1);
        assert_eq!(view.data_length(), 4096);
        assert_eq!(view.audio_id(), AudioId::new(7));
        assert_eq!(chapters(&view), [0, 27]);
    }

    #[test]
    fn chapter_pages_replays_the_same_list_from_a_clone() {
        let view = HeaderView::parse(REAL_1).unwrap();
        let pages = view.chapter_pages();
        let replay = pages.clone();

        assert_eq!(pages.count(), view.chapter_count());
        assert_eq!(replay.map(BlockIndex::get).collect::<Vec<_>>(), [0, 27, 55]);
    }

    #[test]
    fn chapter_pages_ends_rather_than_panicking_on_bytes_that_do_not_decode() {
        // Parsing validates the range, so this state is unreachable through `parse`.
        let mut pages = ChapterPages { rest: &[0x80] };

        assert_eq!(pages.next(), None);
    }

    #[test]
    fn every_error_says_what_went_wrong_and_where() {
        let rendered = [
            HeaderError::WrongBlockLen,
            HeaderError::PrefixOutOfRange,
            HeaderError::TruncatedField,
            HeaderError::MissingField(1),
            HeaderError::DuplicateField(2),
            HeaderError::UnexpectedWireType(3),
            HeaderError::VarintOverflow,
            HeaderError::FieldOutOfRange(4),
        ]
        .map(|error| alloc::format!("{error}"));

        assert_eq!(
            rendered,
            [
                "TAF header block is not 4096 bytes long",
                "TAF header message is longer than 4092 bytes",
                "a TAF header field reaches past the end of the header message",
                "TAF header field 1 is missing",
                "TAF header field 2 appears more than once",
                "TAF header field 3 has an unreadable wire type",
                "a TAF header varint is wider than its field",
                "TAF header field 4 holds an out-of-range value",
            ]
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn header_error_is_a_standard_error() {
        let error: &dyn std::error::Error = &HeaderError::WrongBlockLen;

        assert_eq!(
            std::string::ToString::to_string(error),
            "TAF header block is not 4096 bytes long"
        );
    }
}
