//! The 4096-byte header block that starts every TAF file, read without copying and written
//! without allocating.
//!
//! A header block is a four-byte big-endian length prefix, a protobuf message of that length,
//! and zero padding out to the end of the block. [`HeaderView::parse`] checks the framing once
//! and then borrows the block: the hash stays where it is and the chapter list is decoded while
//! it is iterated. [`encode_header`] goes the other way, sizing the message's fill so the block
//! comes out exactly [`BLOCK_LEN`] bytes. `FORMAT.md` in this crate describes the wire format
//! and is authoritative.

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

/// The bytes a field's tag occupies: one, since every field number this format uses is below 16.
const TAG_LEN: usize = 1;

/// The bytes field 1 occupies: its tag, the one byte that states 20, and the hash itself.
const SHA1_FIELD_LEN: usize = TAG_LEN + 1 + SHA1_LEN;

/// The least field 5 can occupy: its tag and the one-byte length of an empty fill.
const MIN_FILL_FIELD_LEN: usize = TAG_LEN + 1;

/// The largest length a one-byte varint states.
const ONE_BYTE_LEN_MAX: usize = 127;

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

/// The tag that introduces a field: its number lifted above the three bits that carry the wire
/// type, with the wire type in them.
const fn tag(field: u32, wire: u32) -> u32 {
    (field << WIRE_TYPE_BITS) + wire
}

/// The tag that introduces field 1, and the four below it for the fields that follow.
///
/// The format fixes which wire type each field has, so a writer settles that here rather than at
/// every field it writes; a reader takes tags apart again instead, which is what lets it read
/// fields it has no constant for at all.
const TAG_SHA1: u32 = tag(FIELD_SHA1, WIRE_LEN_DELIMITED);
/// The tag that introduces field 2.
const TAG_NUM_BYTES: u32 = tag(FIELD_NUM_BYTES, WIRE_VARINT);
/// The tag that introduces field 3.
const TAG_AUDIO_ID: u32 = tag(FIELD_AUDIO_ID, WIRE_VARINT);
/// The tag that introduces field 4.
const TAG_TRACK_PAGE_NUMS: u32 = tag(FIELD_TRACK_PAGE_NUMS, WIRE_LEN_DELIMITED);
/// The tag that introduces field 5.
const TAG_FILL: u32 = tag(FIELD_FILL, WIRE_LEN_DELIMITED);

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

/// Why a header block could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeHeaderError {
    /// The chapter list leaves the rest of the message no room inside a block.
    TooManyChapters,
}

impl fmt::Display for EncodeHeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::TooManyChapters => write!(
                f,
                "TAF header chapter list does not fit a {MAX_MESSAGE_LEN}-byte header message"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EncodeHeaderError {}

/// Writes the header block that starts a TAF file.
///
/// The block is [`BLOCK_LEN`] bytes: the big-endian length prefix, the protobuf message, and the
/// zero fill that pads it out. `sha1` is the hash of the audio region, `data_length` its length
/// in bytes, and `chapter_pages` the blocks its chapters start at — [`BlockIndex::get`] turns
/// parsed chapter starts back into the raw numbers this takes.
///
/// The message carries the five fields the format requires and nothing else. teddycloud appends
/// four fields of its own bookkeeping behind the fill, which nothing but its own append path
/// reads, so this spends those bytes on fill instead; a block written here therefore parses to
/// the same values as the one teddycloud would write, but not to the same bytes. An empty
/// `chapter_pages` writes no chapter field at all — an empty packed field is written by writing
/// nothing — and [`HeaderView::parse`] reads that back as no chapters.
///
/// The fill is sized so the message fills the block exactly, except in the one case where no
/// fill length lands on it: then the message is a byte shorter, the prefix says so, and the
/// block's last byte is padding. teddycloud's writer has the same one-byte wobble, and readers
/// accept both.
///
/// How many chapters a file may have is a question for whoever writes the file — the format's
/// own limit is 99 — so only what a block can hold is checked here.
///
/// # Errors
///
/// [`EncodeHeaderError::TooManyChapters`] if the packed chapter list and the fields around it
/// leave the fill no room in the 4092 bytes a header message has. That takes eight hundred
/// chapters at the very least, far past the 99 the format allows.
pub fn encode_header(
    sha1: &[u8; SHA1_LEN],
    data_length: u32,
    audio_id: AudioId,
    chapter_pages: &[u32],
) -> Result<[u8; BLOCK_LEN], EncodeHeaderError> {
    let mut scratch = [0; varint::MAX_U32_LEN];
    let packed_len = packed_chapters_len(chapter_pages);
    let chapter_field_len = if chapter_pages.is_empty() {
        0
    } else {
        TAG_LEN + varint::encode_u32(as_u32(packed_len), &mut scratch).len() + packed_len
    };
    let fields_len = SHA1_FIELD_LEN
        + TAG_LEN
        + varint::encode_u32(data_length, &mut scratch).len()
        + TAG_LEN
        + varint::encode_u32(audio_id.get(), &mut scratch).len()
        + chapter_field_len;

    // The fill costs at least its tag and the one-byte length of an empty fill, and a message
    // that cannot pay even that is not a header block.
    if fields_len + MIN_FILL_FIELD_LEN > MAX_MESSAGE_LEN {
        return Err(EncodeHeaderError::TooManyChapters);
    }

    // What the message has left for the fill once the fill's tag is paid for. The length varint
    // in front of the fill eats into that room too: a one-byte length states payloads up to 127
    // and so covers up to 128 bytes of room, a two-byte length states 128 and up and so covers
    // 130 and up. Exactly 129 bytes of room is what neither covers.
    let room = MAX_MESSAGE_LEN - fields_len - TAG_LEN;
    let (fill_len, message_len) = if room <= ONE_BYTE_LEN_MAX + 1 {
        (room - 1, MAX_MESSAGE_LEN)
    } else if room >= ONE_BYTE_LEN_MAX + 3 {
        (room - 2, MAX_MESSAGE_LEN)
    } else {
        // The message lands a byte short of the block, which teddycloud's own sizing does here
        // as well, and the byte it leaves stays padding.
        (ONE_BYTE_LEN_MAX, MAX_MESSAGE_LEN - 1)
    };

    let mut writer = BlockWriter::new();
    writer.put_varint(TAG_SHA1);
    writer.put_len(SHA1_LEN);
    writer.put(sha1);
    writer.put_varint(TAG_NUM_BYTES);
    writer.put_varint(data_length);
    writer.put_varint(TAG_AUDIO_ID);
    writer.put_varint(audio_id.get());
    if !chapter_pages.is_empty() {
        writer.put_varint(TAG_TRACK_PAGE_NUMS);
        writer.put_len(packed_len);
        for &page in chapter_pages {
            writer.put_varint(page);
        }
    }
    writer.put_varint(TAG_FILL);
    writer.put_len(fill_len);
    // The fill itself, and the pad byte a message a byte short of the block leaves, are the
    // zeros the block started as.

    Ok(writer.finish(message_len))
}

/// Sums the bytes a chapter list packs into.
///
/// The sum cannot overflow: a slice holds at most `isize::MAX` bytes, so a slice of `u32` holds
/// at most a quarter as many entries, and five bytes apiece still leaves the sum below what a
/// `usize` holds.
fn packed_chapters_len(chapter_pages: &[u32]) -> usize {
    let mut scratch = [0; varint::MAX_U32_LEN];

    chapter_pages
        .iter()
        .map(|&page| varint::encode_u32(page, &mut scratch).len())
        .sum()
}

/// The block being written, and how far into it the writing has come.
///
/// Writing starts behind the length prefix, because the prefix states how long the message after
/// it is and that is only settled once the message has been written.
struct BlockWriter {
    block: [u8; BLOCK_LEN],
    pos: usize,
}

impl BlockWriter {
    /// Starts a block of zeros, positioned behind the length prefix.
    const fn new() -> Self {
        Self {
            block: [0; BLOCK_LEN],
            pos: PREFIX_LEN,
        }
    }

    /// Copies as much of `bytes` as the block still holds in at the cursor, and steps past them.
    ///
    /// The encoder sizes the whole message before it writes a byte of it, so the block always
    /// does hold them; pairing the two off against each other rather than trusting that is what
    /// keeps a block from ever being written past its end.
    fn put(&mut self, bytes: &[u8]) {
        for (slot, &byte) in self.block.iter_mut().skip(self.pos).zip(bytes) {
            *slot = byte;
        }

        self.pos = self.pos.saturating_add(bytes.len());
    }

    /// Writes a varint.
    fn put_varint(&mut self, value: u32) {
        let mut scratch = [0; varint::MAX_U32_LEN];

        self.put(varint::encode_u32(value, &mut scratch));
    }

    /// Writes a length, which the format states as a varint everywhere but the block's prefix.
    fn put_len(&mut self, len: usize) {
        self.put_varint(as_u32(len));
    }

    /// Writes the length prefix in front of the message and hands the finished block over.
    fn finish(mut self, message_len: usize) -> [u8; BLOCK_LEN] {
        self.pos = 0;
        self.put(&as_u32(message_len).to_be_bytes());

        self.block
    }
}

/// Narrows a length to the `u32` the format states lengths as.
///
/// Every length here is one this encoder derived from a block, so none comes anywhere near this
/// wide; clamping rather than wrapping keeps an impossible one from passing for a plausible one.
fn as_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
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

    /// The message length the block's big-endian prefix states.
    fn message_len(block: &[u8; BLOCK_LEN]) -> usize {
        let prefix: [u8; PREFIX_LEN] = block[..PREFIX_LEN].try_into().unwrap();

        usize::try_from(u32::from_be_bytes(prefix)).unwrap()
    }

    /// A chapter list whose packed form is exactly `packed_len` bytes long, which has to be at
    /// least one byte.
    ///
    /// Every entry from 2^21 on packs to four bytes, and an entry packs to `n` bytes from
    /// 2^(7 * (n - 1)) on, so a run of four-byte entries and one that takes whatever is left
    /// over reaches any length at all. The result is measured against this module's own varint
    /// encoder, so a list is never merely assumed to be the size the test that asked for it
    /// reasons about.
    fn chapters_packed_to(packed_len: usize) -> Vec<u32> {
        let mut pages = Vec::new();
        let mut left = packed_len;

        while left > varint::MAX_U32_LEN {
            pages.push(1 << 21);
            left -= 4;
        }
        pages.push(1 << (7 * (left - 1)));

        let packed: usize = pages
            .iter()
            .map(|&page| varint(u64::from(page)).len())
            .sum();
        assert_eq!(packed, packed_len, "the list has to pack to what was asked");

        pages
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

    #[test]
    fn tags_the_five_fields_the_way_every_fixture_does() {
        // The tag bytes observed in front of the fields of all three fixtures.
        assert_eq!(
            [
                TAG_SHA1,
                TAG_NUM_BYTES,
                TAG_AUDIO_ID,
                TAG_TRACK_PAGE_NUMS,
                TAG_FILL
            ],
            [0x0a, 0x10, 0x18, 0x22, 0x2a]
        );
    }

    #[test]
    fn tags_pack_what_the_parser_takes_apart_again() {
        let wires = [WIRE_VARINT, WIRE_FIXED64, WIRE_LEN_DELIMITED, WIRE_FIXED32];

        for field in 1..=16 {
            for wire in wires {
                let key = tag(field, wire);

                assert_eq!(key >> WIRE_TYPE_BITS, field, "field {field}, wire {wire}");
                assert_eq!(
                    key & ((1 << WIRE_TYPE_BITS) - 1),
                    wire,
                    "field {field}, wire {wire}"
                );
            }
        }
    }

    #[test]
    fn encodes_a_header_the_parser_reads_back() {
        let block = encode_header(&[0xaa; SHA1_LEN], 1234, AudioId::new(99), &[0, 7, 19]).unwrap();
        let view = HeaderView::parse(&block).unwrap();

        assert_eq!(view.sha1(), &[0xaa; SHA1_LEN]);
        assert_eq!(view.data_length(), 1234);
        assert_eq!(view.audio_id(), AudioId::new(99));
        assert_eq!(view.chapter_count(), 3);
        assert_eq!(chapters(&view), [0, 7, 19]);
    }

    #[test]
    fn writes_a_block_whose_prefix_states_a_message_that_fills_it() {
        let block = encode_header(&[0xaa; SHA1_LEN], 1234, AudioId::new(99), &[0, 7, 19]).unwrap();

        assert_eq!(block.len(), BLOCK_LEN);
        assert_eq!(block[..PREFIX_LEN], [0x00, 0x00, 0x0f, 0xfc]);
        assert_eq!(message_len(&block), MAX_MESSAGE_LEN);
    }

    #[test]
    fn writes_neither_a_chapter_field_nor_the_fields_teddycloud_appends() {
        // An empty chapter list is written by writing nothing, which is what protobuf does with
        // an empty packed field. Fields 1..=3 then take 22 + 3 + 2 = 27 bytes, so the fill's tag
        // follows them at 27 and states 4092 - 27 - 3 = 4062 zero bytes that reach the block's
        // end — where teddycloud would have written four fields of its own bookkeeping.
        let block = encode_header(&[0xaa; SHA1_LEN], 1234, AudioId::new(99), &[]).unwrap();

        let mut expected = u32::try_from(MAX_MESSAGE_LEN)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        expected.extend(bytes_field(FIELD_SHA1, &[0xaa; SHA1_LEN]));
        expected.extend(varint_field(FIELD_NUM_BYTES, 1234));
        expected.extend(varint_field(FIELD_AUDIO_ID, 99));
        expected.extend(key(FIELD_FILL, WIRE_LEN_DELIMITED));
        expected.extend(varint(4062));

        assert_eq!(block[..expected.len()], expected[..]);
        assert!(block[expected.len()..].iter().all(|&byte| byte == 0));
        assert_eq!(BLOCK_LEN - expected.len(), 4062);

        let view = HeaderView::parse(&block).unwrap();
        assert_eq!(view.chapter_count(), 0);
        assert_eq!(view.chapter_pages().next(), None);
    }

    #[test]
    fn re_encodes_a_parsed_header_into_the_same_values() {
        for original in [&GOLDEN[..BLOCK_LEN], REAL_1, REAL_2] {
            let parsed = HeaderView::parse(original).unwrap();
            let pages = chapters(&parsed);
            let block = encode_header(
                parsed.sha1(),
                parsed.data_length(),
                parsed.audio_id(),
                &pages,
            )
            .unwrap();
            let view = HeaderView::parse(&block).unwrap();

            assert_eq!(view.sha1(), parsed.sha1());
            assert_eq!(view.data_length(), parsed.data_length());
            assert_eq!(view.audio_id(), parsed.audio_id());
            assert_eq!(chapters(&view), pages);
            assert_eq!(message_len(&block), MAX_MESSAGE_LEN);
            // Not the same bytes: teddycloud spends the tail of its block on four fields of its
            // own bookkeeping, and this crate spends it on fill.
            assert_ne!(block[..], original[..]);
        }
    }

    #[test]
    fn fills_the_block_for_every_short_chapter_list() {
        for count in 0..=64_u32 {
            // Narrow entries and wide ones, so both the packed run's own length varint and the
            // entries in it change width across the sweep.
            let narrow: Vec<u32> = (0..count).collect();
            let wide: Vec<u32> = (0..count).map(|page| u32::MAX - page).collect();

            for (width, pages) in [("narrow", narrow), ("wide", wide)] {
                let block =
                    encode_header(&SHA1, 110_592, AudioId::new(444_913_029), &pages).unwrap();
                let view = HeaderView::parse(&block).unwrap();

                assert_eq!(block.len(), BLOCK_LEN);
                assert_eq!(
                    message_len(&block),
                    MAX_MESSAGE_LEN,
                    "{count} {width} chapters"
                );
                assert_eq!(view.chapter_count(), pages.len());
                assert_eq!(chapters(&view), pages);
            }
        }
    }

    #[test]
    fn lands_one_byte_short_only_where_no_fill_length_fits_the_room_left() {
        // Fields 1..=3 take 22 + 3 + 2 = 27 bytes for these values and field 4 adds its tag and
        // a two-byte length in front of its payload, so a packed list of `packed` bytes leaves
        // the fill 4092 - (30 + packed) - 1 bytes of room behind its own tag. A one-byte length
        // covers up to 128 bytes of that room and a two-byte one covers 130 and up, so 129 —
        // exactly 3932 packed bytes, the size teddycloud's own sizing wobbles at — is the one
        // amount of room neither covers.
        let cases = [
            (3931, MAX_MESSAGE_LEN),
            (3932, MAX_MESSAGE_LEN - 1),
            (3933, MAX_MESSAGE_LEN),
        ];

        for (packed, expected) in cases {
            let pages = chapters_packed_to(packed);
            let block = encode_header(&SHA1, 4096, AudioId::new(1), &pages).unwrap();
            let view = HeaderView::parse(&block).unwrap();

            assert_eq!(message_len(&block), expected, "{packed} packed bytes");
            assert_eq!(chapters(&view), pages, "{packed} packed bytes");
        }
    }

    #[test]
    fn pads_the_block_out_when_the_message_lands_one_byte_short() {
        let pages = chapters_packed_to(3932);
        let block = encode_header(&SHA1, 4096, AudioId::new(1), &pages).unwrap();

        // The fill states 127 bytes, one less than the 128 whose two-byte length would have
        // overshot, and the block's last byte is the padding that leaves.
        let fill_tag = PREFIX_LEN + 30 + 3932;
        assert_eq!(block[fill_tag..fill_tag + 2], [0x2a, 0x7f]);
        assert!(block[fill_tag + 2..].iter().all(|&byte| byte == 0));
        assert_eq!(BLOCK_LEN - (fill_tag + 2), 127 + 1);
        assert_eq!(message_len(&block), MAX_MESSAGE_LEN - 1);
    }

    #[test]
    fn fits_the_longest_chapter_list_a_block_holds() {
        // Fields 1..=3 take 27 bytes here and field 4 adds three more in front of its payload,
        // so 4060 packed bytes put the fields at 4090 — the most that still leaves field 5 its
        // tag and the one-byte length of an empty fill.
        let pages = chapters_packed_to(4060);
        let block = encode_header(&SHA1, 4096, AudioId::new(1), &pages).unwrap();
        let view = HeaderView::parse(&block).unwrap();

        assert_eq!(message_len(&block), MAX_MESSAGE_LEN);
        assert_eq!(chapters(&view), pages);
        // The fill is empty: its tag and its zero length are the block's last two bytes.
        assert_eq!(block[BLOCK_LEN - 2..], [0x2a, 0x00]);
    }

    #[test]
    fn rejects_a_chapter_list_that_leaves_the_fill_no_room() {
        // One packed byte more than the block holds, and a list far past any block.
        let one_too_many = chapters_packed_to(4061);
        let far_too_many = vec![0_u32; BLOCK_LEN];

        for pages in [one_too_many, far_too_many] {
            let count = pages.len();

            assert_eq!(
                encode_header(&SHA1, 4096, AudioId::new(1), &pages).unwrap_err(),
                EncodeHeaderError::TooManyChapters,
                "{count} chapters"
            );
        }
    }

    #[test]
    fn every_encode_error_says_what_went_wrong() {
        assert_eq!(
            alloc::format!("{}", EncodeHeaderError::TooManyChapters),
            "TAF header chapter list does not fit a 4092-byte header message"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn encode_header_error_is_a_standard_error() {
        let error: &dyn std::error::Error = &EncodeHeaderError::TooManyChapters;

        assert_eq!(
            std::string::ToString::to_string(error),
            "TAF header chapter list does not fit a 4092-byte header message"
        );
    }
}
