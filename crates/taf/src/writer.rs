//! Writing a TAF file's audio region a packet at a time, and the header block that describes it.
//!
//! [`TafWriter`] takes Opus packets and hands back pages: the two a stream opens with, and then
//! one per block of the file. What makes those pages a TAF rather than a plain Ogg stream is where
//! they end — the two header pages and the first audio page share the first block, and every page
//! after them is a block of its own — so the writer decides how many packets a page carries and
//! pads the last of them until the page closes its block. `FORMAT.md` in this crate spells that
//! arithmetic out and is authoritative.
//!
//! The writer never holds the file: pages go to the function it was handed as they are finished,
//! and [`finalize`](TafWriter::finalize) hands over the header block for whoever does hold the file
//! to write at offset 0. Under `std`, [`write_taf`] does that part too — it reserves the block at
//! the front of the file and seeks back to fill it in.
//!
//! One thing here is deliberately not what teddycloud does: teddycloud drops the packets it has
//! buffered when it closes a file, up to a block of audio, because it never pads that last page.
//! This writer pads it like any other, so every packet handed in is in the file. Nothing else about
//! the bytes differs — fed the packets of a teddycloud file, it writes that file's audio region
//! byte for byte.

use core::fmt;

use alloc::vec;
use alloc::vec::Vec;

use crate::digest::Sha1;
use crate::header::{self, encode_header, EncodeHeaderError};
use crate::id::AudioId;
use crate::ogg::{
    opus_head, opus_tags, packet_cost, BuildError, PageBuilder, HEADER_LEN, OPUS_HEAD_LEN,
    OPUS_PRE_SKIP, OPUS_TAGS_LEN, PAGE_LEN, SEGMENT_LEN,
};
use crate::opus_packet::{pad_to, PadError};

/// The longest audio region a TAF may carry: `INT32_MAX - 4096` bytes.
///
/// teddycloud bounds a file at this and states it as the header's length field before it knows the
/// real one. Pages land on block boundaries, so the largest file this writer produces stops a block
/// short of it, at 524 286 blocks.
const MAX_AUDIO_LEN: u32 = 2_147_479_551;

/// The bytes one TAF block occupies, counted the way a file's own length counts them.
///
/// The same 4096 as [`header::BLOCK_LEN`] and [`PAGE_LEN`], in the `u32` that the audio region's
/// length and its chapter starts are stated in.
const BLOCK_LEN: u32 = 4096;

/// The bytes the `OpusHead` page occupies: the page header, the one lacing value describing the
/// packet, and the packet itself.
const HEAD_PAGE_LEN: usize = HEADER_LEN + 1 + OPUS_HEAD_LEN;

/// The bytes the `OpusTags` page occupies: the page header, the two lacing values a 436-byte
/// packet takes, and the packet itself.
const TAGS_PAGE_LEN: usize = HEADER_LEN + 2 + OPUS_TAGS_LEN;

/// The bytes the first audio page occupies: what the two pages carrying the Opus headers leave of
/// the block they share with it.
///
/// 47 + 465 + 3584 = 4096, which is the whole reason the `OpusTags` packet has a fixed length.
/// Every page after this one is a block of its own.
const FIRST_AUDIO_PAGE_LEN: usize = PAGE_LEN - HEAD_PAGE_LEN - TAGS_PAGE_LEN;

/// What a full segment occupies on a page: its 255 bytes and the lacing value stating them.
const SEGMENT_STEP: usize = SEGMENT_LEN + 1;

/// The block every TAF starts a chapter at.
const FIRST_CHAPTER: u32 = 0;

/// The longest packet that fits `room` bytes of a page, its lacing values counted in.
///
/// [`packet_cost`] the other way round: a packet of `b` bytes occupies `b + b / 255 + 1` bytes, so
/// every full 256 bytes of room carries 255 bytes of packet, and what is left over carries all but
/// one of itself. teddycloud sizes the frames it hands its encoder with exactly this arithmetic.
///
/// Not every room has a packet that fills it to the byte: 256 bytes of room take a packet of 254,
/// which occupies 255. `packet_cost(packet_filling(room)) == room` is what says whether this room
/// is one of them — teddycloud makes the same comparison before it encodes.
const fn packet_filling(room: usize) -> usize {
    ((room / SEGMENT_STEP) * SEGMENT_LEN + room % SEGMENT_STEP).saturating_sub(1)
}

/// Why a TAF could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriterError {
    /// The packet does not fit a page of this file, even an empty one. A page carries 4053 bytes
    /// at most, and the first audio page — which is shorter, because it shares its block with the
    /// two Opus header pages — carries 3543.
    PacketTooLarge,
    /// The audio region would grow past the 2 147 479 551 bytes the format allows.
    AudioTooLarge,
    /// The last packet of a page could not be padded out to close the page.
    Pad(PadError),
    /// A page, or the `OpusTags` packet the file opens with, could not be built.
    Build(BuildError),
    /// The header block could not be written.
    Header(EncodeHeaderError),
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::PacketTooLarge => f.write_str("an Opus packet is longer than a TAF page carries"),
            Self::AudioTooLarge => write!(
                f,
                "a TAF's audio region cannot grow past {MAX_AUDIO_LEN} bytes"
            ),
            Self::Pad(error) => fmt::Display::fmt(&error, f),
            Self::Build(error) => fmt::Display::fmt(&error, f),
            Self::Header(error) => fmt::Display::fmt(&error, f),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PacketTooLarge | Self::AudioTooLarge => None,
            Self::Pad(error) => Some(error),
            Self::Build(error) => Some(error),
            Self::Header(error) => Some(error),
        }
    }
}

/// What the `OpusTags` packet of a file says: who wrote the stream, and what about it is worth
/// stating.
///
/// The packet is always 436 bytes whatever this says — a `pad=` comment takes up the rest — so
/// nothing here changes where the file's pages fall. Comments are the `NAME=value` strings a Vorbis
/// comment block holds; teddycloud writes the vendor `teddyCloud` and one `version=` comment.
#[derive(Debug, Clone, Copy)]
pub struct Tags<'a> {
    vendor: &'a str,
    comments: &'a [&'a str],
}

impl<'a> Tags<'a> {
    /// States the vendor string and the comments a file's `OpusTags` packet carries.
    #[must_use]
    pub const fn new(vendor: &'a str, comments: &'a [&'a str]) -> Self {
        Self { vendor, comments }
    }
}

/// Writes a TAF's audio region, page by page, out of the Opus packets it is handed.
///
/// Creating one emits the two pages carrying the Opus headers straight away; every
/// [`add_packet`](TafWriter::add_packet) after that emits a page whenever one is full, and
/// [`finalize`](TafWriter::finalize) emits what is left and hands over the header block. Every page
/// goes to the `emit` function as one slice, and they come to 47, 465 and 3584 bytes — the first
/// block — and then a block apiece.
///
/// The packets are the caller's: nothing here encodes audio or checks that a packet is Opus at all.
/// What the writer does with them is decide where pages end, which means padding the last packet of
/// a page (RFC 6716 packet padding, see [`pad_to`](crate::opus_packet::pad_to)) until the page
/// closes its block.
pub struct TafWriter<D: Sha1, F: FnMut(&[u8])> {
    stream: Stream<D>,
    emit: F,
}

impl<D: Sha1, F: FnMut(&[u8])> TafWriter<D, F> {
    /// Starts a file of `audio_id`, hashing everything it writes into `digest`, and emits the two
    /// pages the Opus stream opens with.
    ///
    /// The `OpusHead` page states the pre-skip every TAF states, and the `OpusTags` page says what
    /// `tags` says. Both are emitted before this returns, so a writer that exists has written 512
    /// bytes of audio region. The file's first chapter is recorded at block 0, where every TAF
    /// starts one.
    ///
    /// # Errors
    ///
    /// [`WriterError::Build`] if `tags` does not fit the 436-byte `OpusTags` packet. Nothing is
    /// emitted then.
    pub fn new(
        digest: D,
        audio_id: AudioId,
        tags: Tags<'_>,
        mut emit: F,
    ) -> Result<Self, WriterError> {
        let stream = Stream::start(digest, audio_id, tags, &mut emit)?;

        Ok(Self { stream, emit })
    }

    /// Adds `packet` to the file, which carries `samples` samples of one channel.
    ///
    /// The packet goes on the page being filled, or starts the next one when it no longer fits —
    /// and then the page it left is padded out to its full length and emitted. `samples` is what
    /// the granule positions of the pages are counted in; every packet of a TAF carries 2880, the
    /// samples 60 ms holds at 48 kHz.
    ///
    /// # Errors
    ///
    /// - [`WriterError::PacketTooLarge`] if no page of this file could carry the packet.
    /// - [`WriterError::Pad`] if the last packet of the page this one closes cannot be padded, so
    ///   that page cannot be closed.
    /// - [`WriterError::AudioTooLarge`] if the page this one closes would put the audio region past
    ///   what the format allows.
    /// - [`WriterError::Build`] if a page could not be built.
    pub fn add_packet(&mut self, packet: &[u8], samples: u32) -> Result<(), WriterError> {
        self.stream.add_packet(packet, samples, &mut self.emit)
    }

    /// Starts a chapter at the next block.
    ///
    /// The page being filled is padded out and emitted, so the chapter starts a page of its own at
    /// a block boundary, and the block it starts at is recorded for the header. A chapter that
    /// would start where the last one starts — beginning one before any audio has been added, or
    /// twice over — is not recorded twice.
    ///
    /// # Errors
    ///
    /// - [`WriterError::Pad`] if the last packet of the page being filled cannot be padded.
    /// - [`WriterError::AudioTooLarge`] if emitting that page would put the audio region past what
    ///   the format allows.
    /// - [`WriterError::Build`] if the page could not be built.
    pub fn begin_chapter(&mut self) -> Result<(), WriterError> {
        self.stream.begin_chapter(&mut self.emit)
    }

    /// Emits what is left and hands over the header block the file starts with.
    ///
    /// The page being filled is padded out to a full block and emitted like any other — teddycloud
    /// drops what it has buffered here, and this writer does not — so the audio region ends on a
    /// block boundary with every packet in it. The block that comes back states the SHA-1 of every
    /// byte emitted, how many of them there were, the file's audio id and its chapters.
    ///
    /// # Errors
    ///
    /// - [`WriterError::Pad`] if the last packet of the last page cannot be padded.
    /// - [`WriterError::AudioTooLarge`] if that page would put the audio region past what the
    ///   format allows.
    /// - [`WriterError::Build`] if the page could not be built.
    /// - [`WriterError::Header`] if the chapter list does not fit a header block.
    pub fn finalize(self) -> Result<[u8; header::BLOCK_LEN], WriterError> {
        let Self { stream, mut emit } = self;

        stream.finish(&mut emit)
    }
}

/// The Ogg stream being written, and everything the header block is going to state about it.
///
/// All of the writer's arithmetic lives here; what the two writers around it differ in is only
/// where the pages go.
struct Stream<D: Sha1> {
    digest: D,
    audio_id: AudioId,
    /// The sequence number the next page states.
    sequence: u32,
    /// The samples carried by every packet on a page that has been emitted.
    granule: u64,
    /// The bytes of audio region emitted so far.
    written: u32,
    /// The length the page being filled has to come to.
    target: usize,
    /// What that page occupies so far: its header, the lacing values of its packets, and the
    /// packets themselves.
    used: usize,
    /// The packets buffered for it.
    packets: Vec<Packet>,
    /// The blocks the file's chapters start at.
    chapters: Vec<u32>,
}

/// One packet waiting for the page it goes on to be finished.
struct Packet {
    bytes: Vec<u8>,
    samples: u32,
}

impl<D: Sha1> Stream<D> {
    /// Starts the stream and emits the two pages carrying the Opus headers.
    fn start(
        digest: D,
        audio_id: AudioId,
        tags: Tags<'_>,
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<Self, WriterError> {
        // Both packets are built before either page is written, so tags that do not fit leave no
        // half-written file behind them.
        let head = opus_head(OPUS_PRE_SKIP);
        let comments = opus_tags(tags.vendor, tags.comments).map_err(WriterError::Build)?;

        let mut stream = Self {
            digest,
            audio_id,
            sequence: 0,
            granule: 0,
            written: 0,
            target: FIRST_AUDIO_PAGE_LEN,
            used: HEADER_LEN,
            packets: Vec::new(),
            chapters: vec![FIRST_CHAPTER],
        };

        // The `OpusHead` page opens the stream, and is the one page of a TAF that states BOS.
        let mut opening = stream.page();
        opening.flags(true, false);
        opening.push_packet(&head).map_err(WriterError::Build)?;
        let opening = opening.finish();
        stream.emit_page(&opening, emit)?;

        let mut naming = stream.page();
        naming.push_packet(&comments).map_err(WriterError::Build)?;
        let naming = naming.finish();
        stream.emit_page(&naming, emit)?;

        Ok(stream)
    }

    /// Puts a packet on the page being filled, closing pages as they fill up.
    fn add_packet(
        &mut self,
        packet: &[u8],
        samples: u32,
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<(), WriterError> {
        let cost = packet_cost(packet.len());

        if cost > self.room() {
            // The page has no room for it, so what is on it is padded out to close it and this
            // packet starts the page after it.
            self.close_page(emit)?;
        }
        if cost > self.room() {
            // Closing that page can have moved its last packet onto this one, and this packet may
            // not fit behind that one — which then gets a page to itself.
            self.close_page(emit)?;
        }
        if cost > self.room() {
            return Err(WriterError::PacketTooLarge);
        }

        self.packets.push(Packet {
            bytes: packet.to_vec(),
            samples,
        });
        self.used += cost;

        // A page filled to the byte has nothing left to pad and is written straight away.
        if self.used >= self.target {
            self.close_page(emit)?;
        }

        Ok(())
    }

    /// Ends the page being filled and records where the chapter that follows it starts.
    fn begin_chapter(&mut self, emit: &mut dyn FnMut(&[u8])) -> Result<(), WriterError> {
        self.flush(emit)?;

        // Every page written so far ended on a block boundary, so what has been written names the
        // block the next page starts. The two Opus header pages are 512 bytes of block 0, which is
        // why every file's first chapter starts there.
        let block = self.written / BLOCK_LEN;

        // A chapter that starts where the last one starts is that same chapter, not another.
        if self.chapters.last() != Some(&block) {
            self.chapters.push(block);
        }

        Ok(())
    }

    /// Emits what is left of the audio region and writes the header block.
    fn finish(
        mut self,
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<[u8; header::BLOCK_LEN], WriterError> {
        self.flush(emit)?;

        let Self {
            digest,
            audio_id,
            written,
            chapters,
            ..
        } = self;

        encode_header(&digest.finalize(), written, audio_id, &chapters).map_err(WriterError::Header)
    }

    /// Closes the page being filled, and the page that close may have started, so that nothing is
    /// left buffered and the audio region ends on a block boundary.
    fn flush(&mut self, emit: &mut dyn FnMut(&[u8])) -> Result<(), WriterError> {
        self.close_page(emit)?;
        // Closing a page can move its last packet onto the next one — see [`Stream::page_split`] —
        // and that page is closed in turn. A page holding a single packet is never one that moves
        // it, so closing twice always empties the buffer.
        self.close_page(emit)
    }

    /// Pads the page being filled out to its full length, writes it, and starts the next one.
    ///
    /// A page with nothing on it is not written at all, which is what closing a page comes to when
    /// the one before it came out full.
    fn close_page(&mut self, emit: &mut dyn FnMut(&[u8])) -> Result<(), WriterError> {
        let (taken, room) = self.page_split();
        let Some((last, head)) = self.packets.get(..taken).and_then(<[Packet]>::split_last) else {
            return Ok(());
        };

        // What the last packet has to grow to for the page to come to its target length. A packet
        // already that long — the one that filled the page exactly — is written as it is.
        let filling = packet_filling(room);
        let padded = if last.bytes.len() < filling {
            Some(pad_to(&last.bytes, filling).map_err(WriterError::Pad)?)
        } else {
            None
        };

        // The granule position states the samples of every packet the file carries up to and
        // including this page — the ones this page does not take are the next page's.
        let granule = head
            .iter()
            .chain([last])
            .fold(self.granule, |granule, packet| {
                granule.saturating_add(u64::from(packet.samples))
            });

        let mut page = self.page();
        page.granule_position(granule);
        for packet in head {
            page.push_packet(&packet.bytes)
                .map_err(WriterError::Build)?;
        }
        page.push_packet(padded.as_deref().unwrap_or(&last.bytes))
            .map_err(WriterError::Build)?;
        let page = page.finish();

        self.emit_page(&page, emit)?;
        self.granule = granule;
        // Only the first audio page is short: it closes the block the two Opus header pages start.
        self.target = PAGE_LEN;
        // Whatever the page did not take — see `page_split`, which never names more packets than
        // are buffered — starts the next one.
        self.packets.drain(..taken);
        self.used = self.packets.iter().fold(HEADER_LEN, |used, packet| {
            used + packet_cost(packet.bytes.len())
        });

        Ok(())
    }

    /// How many of the buffered packets the page being filled takes, and the room the last of them
    /// has to fill.
    ///
    /// A page takes every packet buffered for it, with one exception: the packet it cannot be
    /// closed with. A packet of `b` bytes occupies `b + b / 255 + 1` bytes of a page, which is
    /// never a multiple of 256, so a packet pushed into a room that *is* a multiple of 256 can
    /// never be padded to end the page on the byte. That packet moves onto the next page, and the
    /// packet before it closes this one instead. The first packet of a page has 3557 or 4069 bytes
    /// of room, neither of them a multiple of 256, so a page always keeps a packet it can be closed
    /// with.
    fn page_split(&self) -> (usize, usize) {
        let mut room = self.target.saturating_sub(HEADER_LEN);
        let mut taken = 0;
        let mut filled = 0;

        for (at, packet) in self.packets.iter().enumerate() {
            if packet_cost(packet_filling(room)) == room {
                taken = at + 1;
                filled = room;
            }

            room = room.saturating_sub(packet_cost(packet.bytes.len()));
        }

        (taken, filled)
    }

    /// Hands a finished page over, and counts it into everything the header block will state.
    ///
    /// # Errors
    ///
    /// [`WriterError::AudioTooLarge`] if the page would put the audio region past what a header
    /// states. The page is not emitted then, and neither is any page after it.
    fn emit_page(&mut self, page: &[u8], emit: &mut dyn FnMut(&[u8])) -> Result<(), WriterError> {
        let written = u32::try_from(page.len())
            .ok()
            .and_then(|len| self.written.checked_add(len))
            .filter(|&written| written <= MAX_AUDIO_LEN)
            .ok_or(WriterError::AudioTooLarge)?;

        emit(page);
        self.digest.update(page);
        self.written = written;
        // Half a million pages is the most a file this length holds, so this counts every one.
        self.sequence = self.sequence.saturating_add(1);

        Ok(())
    }

    /// An empty page at the front of what has not been written yet.
    fn page(&self) -> PageBuilder {
        PageBuilder::new(self.audio_id.get(), self.sequence)
    }

    /// What is left of the page being filled: the bytes one more packet may occupy on it.
    fn room(&self) -> usize {
        self.target.saturating_sub(self.used)
    }
}

/// The `std` half of the crate's writer: the same file, written into something that seeks.
#[cfg(feature = "std")]
mod std_io {
    use core::fmt;
    use std::io::{Seek, SeekFrom, Write};

    use super::{header, AudioId, Sha1, Stream, Tags, WriterError};

    /// Why a TAF could not be written to a file.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum WriterIoError {
        /// The file could not be written the way a TAF states it.
        Writer(WriterError),
        /// Writing to, or seeking in, the file itself failed.
        Io(std::io::Error),
    }

    impl fmt::Display for WriterIoError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Writer(error) => fmt::Display::fmt(error, f),
                Self::Io(error) => fmt::Display::fmt(error, f),
            }
        }
    }

    impl std::error::Error for WriterIoError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::Writer(error) => Some(error),
                Self::Io(error) => Some(error),
            }
        }
    }

    impl From<WriterError> for WriterIoError {
        fn from(error: WriterError) -> Self {
            Self::Writer(error)
        }
    }

    impl From<std::io::Error> for WriterIoError {
        fn from(error: std::io::Error) -> Self {
            Self::Io(error)
        }
    }

    /// Writes a whole TAF file into `out`, header block and all.
    ///
    /// The header block is only known once the last packet is in, so the space for it is written as
    /// zeros here and filled in by [`StdTafWriter::finalize`], which seeks back to the front of the
    /// file. The two pages carrying the Opus headers are written straight away, as with
    /// [`TafWriter`](super::TafWriter).
    ///
    /// # Errors
    ///
    /// - [`WriterIoError::Io`] if reserving the header block or writing the first two pages fails.
    /// - [`WriterIoError::Writer`] if `tags` does not fit the 436-byte `OpusTags` packet.
    pub fn write_taf<D: Sha1, W: Write + Seek>(
        digest: D,
        audio_id: AudioId,
        tags: Tags<'_>,
        out: W,
    ) -> Result<StdTafWriter<D, W>, WriterIoError> {
        let mut out = out;
        // Written rather than seeked over, so that a file left unfinished holds zeros where its
        // header goes rather than whatever the file it is being written over held there.
        out.write_all(&[0; header::BLOCK_LEN])?;

        let mut error = None;
        let started = Stream::start(digest, audio_id, tags, &mut sink(&mut out, &mut error));

        outcome(error, started).map(|stream| StdTafWriter {
            stream,
            out,
            error: None,
        })
    }

    /// A TAF being written into something that can be written to and seeked in.
    ///
    /// The same writer as [`TafWriter`](super::TafWriter), with the file at the other end of it: it
    /// takes the same packets and chapters, and its [`finalize`](StdTafWriter::finalize) writes the
    /// header block rather than handing it over. [`write_taf`] creates one.
    pub struct StdTafWriter<D: Sha1, W: Write + Seek> {
        stream: Stream<D>,
        out: W,
        error: Option<std::io::Error>,
    }

    impl<D: Sha1, W: Write + Seek> StdTafWriter<D, W> {
        /// Adds `packet` to the file, which carries `samples` samples of one channel.
        ///
        /// # Errors
        ///
        /// - [`WriterIoError::Io`] if writing a page to the file failed.
        /// - [`WriterIoError::Writer`] for everything
        ///   [`TafWriter::add_packet`](super::TafWriter::add_packet) reports.
        pub fn add_packet(&mut self, packet: &[u8], samples: u32) -> Result<(), WriterIoError> {
            let Self { stream, out, error } = self;
            let added = stream.add_packet(packet, samples, &mut sink(out, error));

            outcome(error.take(), added)
        }

        /// Starts a chapter at the next block.
        ///
        /// # Errors
        ///
        /// - [`WriterIoError::Io`] if writing a page to the file failed.
        /// - [`WriterIoError::Writer`] for everything
        ///   [`TafWriter::begin_chapter`](super::TafWriter::begin_chapter) reports.
        pub fn begin_chapter(&mut self) -> Result<(), WriterIoError> {
            let Self { stream, out, error } = self;
            let begun = stream.begin_chapter(&mut sink(out, error));

            outcome(error.take(), begun)
        }

        /// Finishes the file: writes what is left of the audio region, fills in the header block at
        /// the front, and hands the file back.
        ///
        /// # Errors
        ///
        /// - [`WriterIoError::Io`] if writing the last page, seeking back or writing the header
        ///   block failed.
        /// - [`WriterIoError::Writer`] for everything
        ///   [`TafWriter::finalize`](super::TafWriter::finalize) reports.
        pub fn finalize(self) -> Result<W, WriterIoError> {
            let Self {
                stream,
                mut out,
                mut error,
            } = self;
            let finished = stream.finish(&mut sink(&mut out, &mut error));
            let block = outcome(error.take(), finished)?;

            out.seek(SeekFrom::Start(0))?;
            out.write_all(&block)?;
            out.flush()?;

            Ok(out)
        }
    }

    /// The function pages are emitted to: it writes them to `out` and keeps the first error it
    /// hits, since emitting a page cannot report one itself.
    fn sink<'a, W: Write>(
        out: &'a mut W,
        error: &'a mut Option<std::io::Error>,
    ) -> impl FnMut(&[u8]) + 'a {
        move |page| {
            if error.is_some() {
                return;
            }
            if let Err(hit) = out.write_all(page) {
                *error = Some(hit);
            }
        }
    }

    /// What a call comes to: the error the file reported, if it reported one, and otherwise
    /// whatever the writer itself made of the call.
    fn outcome<T>(
        error: Option<std::io::Error>,
        result: Result<T, WriterError>,
    ) -> Result<T, WriterIoError> {
        match error {
            Some(error) => Err(WriterIoError::Io(error)),
            None => result.map_err(WriterIoError::Writer),
        }
    }
}

#[cfg(feature = "std")]
pub use std_io::{write_taf, StdTafWriter, WriterIoError};

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::header::HeaderView;
    use crate::id::BlockIndex;
    use crate::ogg::PageView;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    const GOLDEN: &[u8] = include_bytes!("../tests/fixtures/golden-sine.taf");

    /// Where a TAF's audio region starts: behind the header block.
    const AUDIO_AT: usize = header::BLOCK_LEN;

    /// The golden file's audio id, which every one of its pages states as its serial number.
    const AUDIO_ID: AudioId = AudioId::new(444_913_029);

    /// The vendor string teddycloud wrote into the golden file.
    const VENDOR: &str = "teddyCloud";

    /// The one comment it wrote beside the padding.
    const COMMENT: &str =
        "version=TeddyCloud v0.6.2 (203f12d) - 2024-10-26 18:14:34 +0000 ubuntu linux-x86_64(64)";

    /// The samples of one channel an Opus packet of a TAF carries: 60 ms at 48 kHz.
    const SAMPLES: u32 = 2880;

    /// A TOC byte stating configuration 1, stereo, and one frame. The packets built here are code 0
    /// packets, whose single frame is everything behind that byte, so a packet of any length at all
    /// is one [`pad_to`] can take apart.
    const TOC: u8 = 0b0000_1100;

    /// The pages a writer has emitted, which a test reads while that writer is still going.
    type Pages = RefCell<Vec<Vec<u8>>>;

    struct RustCrypto(sha1::Sha1);

    impl Sha1 for RustCrypto {
        fn update(&mut self, data: &[u8]) {
            sha1::Digest::update(&mut self.0, data);
        }

        fn finalize(self) -> [u8; 20] {
            sha1::Digest::finalize(self.0).into()
        }
    }

    fn digest() -> RustCrypto {
        RustCrypto(<sha1::Sha1 as sha1::Digest>::new())
    }

    /// The `OpusTags` content teddycloud wrote into the golden file.
    fn tags() -> Tags<'static> {
        Tags::new(VENDOR, &[COMMENT])
    }

    /// A packet of `len` bytes that [`pad_to`] can take apart: a TOC byte and one frame.
    fn packet(len: usize) -> Vec<u8> {
        let mut packet = vec![0xa5; len];
        packet[0] = TOC;

        packet
    }

    /// A writer whose pages land in `pages`, opened with the `OpusTags` content given.
    fn writer_with<'a>(
        pages: &'a Pages,
        tags: Tags<'_>,
    ) -> Result<TafWriter<RustCrypto, impl FnMut(&[u8]) + 'a>, WriterError> {
        TafWriter::new(digest(), AUDIO_ID, tags, |page: &[u8]| {
            pages.borrow_mut().push(page.to_vec());
        })
    }

    /// A writer whose pages land in `pages`, opened with the tags teddycloud writes.
    fn writer(pages: &Pages) -> TafWriter<RustCrypto, impl FnMut(&[u8]) + '_> {
        writer_with(pages, tags()).unwrap()
    }

    /// The lengths of the pages emitted so far.
    fn lens(pages: &Pages) -> Vec<usize> {
        pages.borrow().iter().map(Vec::len).collect()
    }

    /// The audio region emitted so far, one page after the other.
    fn audio(pages: &Pages) -> Vec<u8> {
        pages.borrow().concat()
    }

    /// One emitted page.
    fn page(pages: &Pages, at: usize) -> Vec<u8> {
        pages.borrow()[at].clone()
    }

    /// The packets a page carries, read back the way a reader reads them — which checks the page's
    /// checksum on the way.
    fn packets_of(page: &[u8]) -> Vec<Vec<u8>> {
        PageView::parse(page)
            .unwrap()
            .packets()
            .take(256)
            .map(<[u8]>::to_vec)
            .collect()
    }

    /// The granule position a page states.
    fn granule_of(page: &[u8]) -> u64 {
        PageView::parse(page).unwrap().granule_position()
    }

    /// The chapter starts a header block states.
    fn chapters(block: &[u8; header::BLOCK_LEN]) -> Vec<u32> {
        HeaderView::parse(block)
            .unwrap()
            .chapter_pages()
            .map(BlockIndex::get)
            .collect()
    }

    /// The audio packets of the golden file: everything its pages from sequence 2 on carry.
    ///
    /// How many samples each of them holds is what the granule positions say — every page's
    /// advances by 2880 per packet on it, which is the 60 ms frame teddycloud encodes.
    fn golden_packets() -> Vec<&'static [u8]> {
        let mut packets = Vec::new();
        let mut at = AUDIO_AT;
        let mut granule = 0;

        while at < GOLDEN.len() {
            let view = PageView::parse(&GOLDEN[at..]).unwrap();
            let carried: Vec<&[u8]> = view.packets().take(256).collect();
            let sequence = view.sequence();

            if sequence >= 2 {
                assert_eq!(
                    view.granule_position() - granule,
                    u64::from(SAMPLES) * u64::try_from(carried.len()).unwrap(),
                    "page {sequence}"
                );
                packets.extend(carried);
            }

            granule = view.granule_position();
            at += view.total_len();
        }

        assert_eq!(packets.len(), 161);

        packets
    }

    /// Feeds the golden file's own packets through a writer, and hands back the pages it emitted and
    /// the header block it finished with.
    fn replay_golden(pages: &Pages) -> [u8; header::BLOCK_LEN] {
        let mut writer = writer(pages);

        for packet in golden_packets() {
            writer.add_packet(packet, SAMPLES).unwrap();
        }

        writer.finalize().unwrap()
    }

    #[test]
    fn writes_the_golden_files_audio_region_byte_for_byte() {
        let pages = Pages::default();
        let block = replay_golden(&pages);
        let written = audio(&pages);
        let golden = &GOLDEN[AUDIO_AT..];

        // Which byte is the first to differ, if any does — a length that does not match says
        // nothing about where the two streams parted.
        let diverged = written
            .iter()
            .zip(golden)
            .position(|(written, golden)| written != golden);

        assert_eq!(diverged, None, "the audio regions differ");
        assert_eq!(written.len(), golden.len());

        let golden_header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();
        let view = HeaderView::parse(&block).unwrap();

        assert_eq!(view.sha1(), golden_header.sha1());
        assert_eq!(view.data_length(), golden_header.data_length());
        assert_eq!(view.data_length(), 110_592);
        assert_eq!(view.audio_id(), AUDIO_ID);
        assert_eq!(chapters(&block), [0]);
    }

    #[test]
    fn emits_the_pages_the_format_lays_a_file_out_as() {
        let pages = Pages::default();
        let _ = replay_golden(&pages);
        let lens = lens(&pages);

        // The two pages carrying the Opus headers and the first audio page share the first block;
        // every page after them is a block of its own.
        assert_eq!(lens[..3], [47, 465, 3584]);
        assert_eq!(lens[..3].iter().sum::<usize>(), PAGE_LEN);
        assert!(lens[3..].iter().all(|&len| len == PAGE_LEN));
        assert_eq!(lens.len(), 29);
    }

    #[test]
    fn states_the_stream_the_way_every_page_of_a_taf_states_it() {
        let pages = Pages::default();
        let _ = replay_golden(&pages);
        let mut granule = 0;

        for (sequence, page) in pages.borrow().iter().enumerate() {
            let view = PageView::parse(page).unwrap();

            assert_eq!(view.serial(), AUDIO_ID.get(), "page {sequence}");
            assert_eq!(usize::try_from(view.sequence()), Ok(sequence));
            assert_eq!(view.is_first(), sequence == 0, "page {sequence}");
            assert!(!view.is_last(), "page {sequence}");
            assert!(view.granule_position() >= granule, "page {sequence}");

            granule = view.granule_position();
        }

        // The granule positions the golden file's own pages state: raw samples, with no pre-skip
        // taken off them.
        assert_eq!(granule_of(&page(&pages, 0)), 0);
        assert_eq!(granule_of(&page(&pages, 1)), 0);
        assert_eq!(granule_of(&page(&pages, 2)), 14_400);
        assert_eq!(granule_of(&page(&pages, 3)), 31_680);
        assert_eq!(granule, 463_680);
    }

    #[test]
    fn starts_every_chapter_on_a_block_of_its_own() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        for chapter in 0..3 {
            if chapter > 0 {
                writer.begin_chapter().unwrap();
            }
            for _ in 0..12 {
                writer.add_packet(&packet(700), SAMPLES).unwrap();
            }
        }

        let block = writer.finalize().unwrap();

        // The whole file, read back the way anything that reads a TAF reads it.
        let mut file = block.to_vec();
        file.extend(audio(&pages));

        let view = HeaderView::parse(&block).unwrap();
        let starts = chapters(&block);

        assert_eq!(starts, [0, 3, 6]);
        assert_eq!(
            usize::try_from(view.data_length()),
            Ok(file.len() - AUDIO_AT)
        );
        assert_eq!(view.data_length() % BLOCK_LEN, 0);

        for &start in &starts {
            // Block `n` starts at file offset 4096 * (n + 1), and block 0's audio starts behind the
            // two Opus header pages that share the block with it.
            let headers = if start == 0 {
                HEAD_PAGE_LEN + TAGS_PAGE_LEN
            } else {
                0
            };
            let at = AUDIO_AT + usize::try_from(start).unwrap() * PAGE_LEN + headers;
            let page = PageView::parse(&file[at..]).unwrap();

            assert_eq!(page.serial(), AUDIO_ID.get(), "chapter at block {start}");
            assert!(!page.is_last(), "chapter at block {start}");
        }

        // Every page of the file, walked from the front: contiguous sequence numbers, a block
        // apiece once the first one is behind them, and nothing that ends the stream.
        let mut at = AUDIO_AT;
        let mut sequence = 0;

        while at < file.len() {
            let view = PageView::parse(&file[at..]).unwrap();

            assert_eq!(view.sequence(), sequence, "page at {at}");
            if at >= AUDIO_AT + PAGE_LEN {
                assert_eq!(at % PAGE_LEN, 0, "page at {at}");
                assert_eq!(view.total_len(), PAGE_LEN, "page at {at}");
            }

            at += view.total_len();
            sequence += 1;
        }

        assert_eq!(at, file.len());
    }

    #[test]
    fn does_not_record_a_chapter_where_one_already_starts() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // Before any audio at all: the file already starts a chapter at block 0.
        writer.begin_chapter().unwrap();
        writer.add_packet(&packet(700), SAMPLES).unwrap();
        writer.begin_chapter().unwrap();
        // And again, with nothing added in between.
        writer.begin_chapter().unwrap();
        writer.add_packet(&packet(700), SAMPLES).unwrap();

        let block = writer.finalize().unwrap();

        assert_eq!(chapters(&block), [0, 1]);
        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN]);
    }

    #[test]
    fn pads_the_last_page_rather_than_dropping_what_it_holds() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        for _ in 0..3 {
            writer.add_packet(&packet(700), SAMPLES).unwrap();
        }

        let block = writer.finalize().unwrap();
        let handed_in = packet(700);
        let carried = packets_of(&page(&pages, 2));

        // Three packets are nowhere near a full page, and the page is written all the same: every
        // packet handed in is in the file, and the file still ends on a block boundary.
        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN]);
        assert_eq!(audio(&pages).len() % PAGE_LEN, 0);
        assert_eq!(carried.len(), 3);
        assert_eq!(carried[0], handed_in);
        assert_eq!(carried[1], handed_in);
        // The last of them is what closes the page: the same packet, padded to the room it had.
        assert!(carried[2].len() > handed_in.len());
        assert_eq!(carried[2], pad_to(&handed_in, carried[2].len()).unwrap());
        assert_eq!(granule_of(&page(&pages, 2)), u64::from(SAMPLES) * 3);

        let view = HeaderView::parse(&block).unwrap();
        assert_eq!(usize::try_from(view.data_length()), Ok(audio(&pages).len()));
    }

    #[test]
    fn pads_the_last_packet_of_a_page_the_next_one_no_longer_fits() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // Five packets of 700 bytes occupy 3515 of the 3557 bytes the first audio page has for
        // them, and a sixth does not fit the 42 that leaves — so the fifth is padded out to close
        // the page and the sixth starts the one after it.
        for _ in 0..6 {
            writer.add_packet(&packet(700), SAMPLES).unwrap();
        }

        let carried = packets_of(&page(&pages, 2));

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN]);
        assert_eq!(carried.len(), 5);
        assert_eq!(carried[4].len(), 742);
        assert_eq!(carried[4], pad_to(&packet(700), 742).unwrap());
        assert_eq!(granule_of(&page(&pages, 2)), u64::from(SAMPLES) * 5);

        // The sixth is on the page being filled, and is written when that page is closed.
        let _ = writer.finalize().unwrap();

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN]);
        assert_eq!(packets_of(&page(&pages, 3)).len(), 1);
    }

    #[test]
    fn fills_a_page_exactly_without_padding_anything() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // A packet of 3543 bytes occupies exactly the 3557 the first audio page has, so the page is
        // full the moment it goes on and nothing about it needs padding.
        writer.add_packet(&packet(3543), SAMPLES).unwrap();

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN]);
        assert_eq!(packets_of(&page(&pages, 2)), [packet(3543)]);

        // And the same for a whole page: 4053 bytes occupy the 4069 one has.
        writer.add_packet(&packet(4053), SAMPLES).unwrap();

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN]);
        assert_eq!(packets_of(&page(&pages, 3)), [packet(4053)]);

        // Nothing is buffered, so there is no last page to write.
        let block = writer.finalize().unwrap();

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN]);
        assert_eq!(HeaderView::parse(&block).unwrap().data_length(), 8192);
    }

    #[test]
    fn moves_a_packet_it_cannot_close_a_page_with_onto_the_next_one() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // A packet of 228 bytes occupies 229 of the first audio page's 3557, leaving 3328 — a
        // multiple of 256, which is a room no packet occupies exactly. So the 100-byte packet
        // pushed into it could never close that page: it moves onto the next one, and the 228-byte
        // packet closes this one on its own.
        writer.add_packet(&packet(228), SAMPLES).unwrap();
        writer.add_packet(&packet(100), SAMPLES).unwrap();

        let block = writer.finalize().unwrap();

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN]);
        assert_eq!(
            packets_of(&page(&pages, 2)),
            [pad_to(&packet(228), 3543).unwrap()]
        );
        assert_eq!(
            packets_of(&page(&pages, 3)),
            [pad_to(&packet(100), 4053).unwrap()]
        );

        // The samples of the packet that moved moved with it.
        assert_eq!(granule_of(&page(&pages, 2)), u64::from(SAMPLES));
        assert_eq!(granule_of(&page(&pages, 3)), u64::from(SAMPLES) * 2);
        assert_eq!(chapters(&block), [0]);
    }

    #[test]
    fn gives_a_packet_it_moved_a_page_of_its_own_when_the_next_one_does_not_fit_behind_it() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        writer.add_packet(&packet(228), SAMPLES).unwrap();
        writer.add_packet(&packet(100), SAMPLES).unwrap();
        // 4000 bytes occupy 4016 of a page: more than the 3968 left behind the 100-byte packet that
        // the close above moved onto this page, and less than the 4069 an empty page has. So that
        // packet gets a page to itself, and this one starts the page after it.
        writer.add_packet(&packet(4000), SAMPLES).unwrap();

        let block = writer.finalize().unwrap();

        assert_eq!(
            lens(&pages),
            [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN, PAGE_LEN]
        );
        assert_eq!(
            packets_of(&page(&pages, 3)),
            [pad_to(&packet(100), 4053).unwrap()]
        );
        assert_eq!(
            packets_of(&page(&pages, 4)),
            [pad_to(&packet(4000), 4053).unwrap()]
        );
        assert_eq!(HeaderView::parse(&block).unwrap().data_length(), 12_288);
    }

    #[test]
    fn refuses_a_packet_no_page_of_the_file_holds() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // The first audio page has 3557 bytes for its packets, so the most it takes is 3543 — one
        // byte more and this page cannot hold it, though a whole page later in the file could.
        assert_eq!(
            writer.add_packet(&packet(3544), SAMPLES),
            Err(WriterError::PacketTooLarge)
        );

        writer.add_packet(&packet(3543), SAMPLES).unwrap();

        // That packet filled the first audio page exactly, so what follows is a whole block: 4069
        // bytes for its packets, and 4053 the most one of them can be.
        assert_eq!(
            writer.add_packet(&packet(4054), SAMPLES),
            Err(WriterError::PacketTooLarge)
        );
        writer.add_packet(&packet(4053), SAMPLES).unwrap();

        assert_eq!(lens(&pages), [47, 465, FIRST_AUDIO_PAGE_LEN, PAGE_LEN]);
    }

    #[test]
    fn reports_a_packet_it_cannot_pad_to_close_a_page() {
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // A code 1 packet carries two frames of one size, so a payload of three bytes is a packet
        // that does not divide into the frames its TOC byte states — and a packet that cannot be
        // taken apart cannot be padded to close the page it ends.
        writer.add_packet(&[TOC | 1, 1, 2, 3], SAMPLES).unwrap();

        // Starting a chapter closes the page too, and says the same thing about it.
        assert_eq!(
            writer.begin_chapter(),
            Err(WriterError::Pad(PadError::MalformedToc))
        );
        assert_eq!(
            writer.finalize(),
            Err(WriterError::Pad(PadError::MalformedToc))
        );
        assert_eq!(lens(&pages), [47, 465]);
    }

    #[test]
    fn refuses_tags_that_do_not_fit_the_packet_they_go_in() {
        let pages = Pages::default();
        let vendor = String::from_utf8(vec![b'v'; 500]).unwrap();
        let refused = writer_with(&pages, Tags::new(&vendor, &[]));

        assert_eq!(
            refused.err(),
            Some(WriterError::Build(BuildError::TagsTooLong))
        );
        assert!(
            pages.borrow().is_empty(),
            "nothing is written before the tags are known to fit"
        );
    }

    #[test]
    fn refuses_audio_past_the_length_a_taf_may_state() {
        // The blocks that still fit under the 2 147 479 551 bytes the format allows: one page more
        // and the file is past it.
        let full = (MAX_AUDIO_LEN / BLOCK_LEN) * BLOCK_LEN;
        let pages = Pages::default();
        let mut writer = writer(&pages);

        // The state a writer is in with one block still to go. The first audio page is long behind
        // it by then, so every page from here is a whole block.
        writer.stream.written = full - BLOCK_LEN;
        writer.stream.target = PAGE_LEN;

        // Six packets of 700 bytes fill a page, and that page still fits.
        for _ in 0..6 {
            writer.add_packet(&packet(700), SAMPLES).unwrap();
        }

        assert_eq!(writer.stream.written, full);
        assert_eq!(lens(&pages).len(), 3);

        // The one after it does not, and nothing is emitted for it.
        let refused = (0..6).try_for_each(|_| writer.add_packet(&packet(700), SAMPLES));

        assert_eq!(refused, Err(WriterError::AudioTooLarge));
        assert_eq!(lens(&pages).len(), 3);
        assert_eq!(full, 2_147_475_456);
    }

    #[test]
    fn refuses_it_however_the_page_that_would_pass_the_bound_is_closed() {
        let full = (MAX_AUDIO_LEN / BLOCK_LEN) * BLOCK_LEN;

        // A page filled to the byte is written the moment it fills up, and this one would be one
        // page too many.
        let pages = Pages::default();
        let mut exactly = writer(&pages);
        exactly.stream.written = full;
        exactly.stream.target = PAGE_LEN;

        assert_eq!(
            exactly.add_packet(&packet(4053), SAMPLES),
            Err(WriterError::AudioTooLarge)
        );
        assert_eq!(lens(&pages), [47, 465]);

        // And so is the page that a close moved a packet onto, once the packet at hand turns out
        // not to fit behind it.
        let pages = Pages::default();
        let mut moved = writer(&pages);
        moved.stream.written = full - BLOCK_LEN;
        moved.stream.target = PAGE_LEN;
        moved.add_packet(&packet(228), SAMPLES).unwrap();
        moved.add_packet(&packet(100), SAMPLES).unwrap();

        assert_eq!(
            moved.add_packet(&packet(4000), SAMPLES),
            Err(WriterError::AudioTooLarge)
        );
        assert_eq!(lens(&pages), [47, 465, PAGE_LEN]);
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        let rendered = [
            WriterError::PacketTooLarge,
            WriterError::AudioTooLarge,
            WriterError::Pad(PadError::MalformedToc),
            WriterError::Build(BuildError::TagsTooLong),
            WriterError::Header(EncodeHeaderError::TooManyChapters),
        ]
        .map(|error| alloc::format!("{error}"));

        assert_eq!(
            rendered,
            [
                "an Opus packet is longer than a TAF page carries",
                "a TAF's audio region cannot grow past 2147479551 bytes",
                "an Opus packet does not divide into the frames its TOC byte states",
                "OpusTags content does not fit a 436-byte packet",
                "TAF header chapter list does not fit a 4092-byte header message",
            ]
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn writer_error_is_a_standard_error_that_names_what_it_wraps() {
        use std::error::Error;

        assert!(WriterError::PacketTooLarge.source().is_none());
        assert!(WriterError::AudioTooLarge.source().is_none());

        for (error, message) in [
            (
                WriterError::Pad(PadError::EmptyPacket),
                "an Opus packet is at least one byte",
            ),
            (
                WriterError::Build(BuildError::PageFull),
                "an Ogg page has no room left for another packet",
            ),
            (
                WriterError::Header(EncodeHeaderError::TooManyChapters),
                "TAF header chapter list does not fit a 4092-byte header message",
            ),
        ] {
            let source = error.source().unwrap();

            assert_eq!(std::string::ToString::to_string(source), message);
        }
    }

    #[test]
    fn the_constants_are_the_ones_the_format_states() {
        assert_eq!(HEAD_PAGE_LEN, 47);
        assert_eq!(TAGS_PAGE_LEN, 465);
        assert_eq!(FIRST_AUDIO_PAGE_LEN, 3584);
        assert_eq!(
            HEAD_PAGE_LEN + TAGS_PAGE_LEN + FIRST_AUDIO_PAGE_LEN,
            PAGE_LEN
        );
        assert_eq!(usize::try_from(BLOCK_LEN), Ok(header::BLOCK_LEN));
        assert_eq!(MAX_AUDIO_LEN, 2_147_479_551);
        assert_eq!(SEGMENT_STEP, 256);
        assert_eq!(FIRST_CHAPTER, 0);
    }

    #[test]
    fn sizes_the_packet_that_closes_a_page_the_way_the_format_document_does() {
        // FORMAT.md's worked example: the first audio page has 3557 bytes for its packets and the
        // largest one that fits is 3543. Neither that room nor the 4069 a whole page has is a
        // multiple of 256, which is why a page can always be closed with its first packet.
        assert_eq!(packet_filling(3557), 3543);
        assert_eq!(packet_cost(3543), 3557);
        assert_eq!(packet_filling(4069), 4053);
        assert_eq!(packet_cost(4053), 4069);

        // The room no packet fills: 256 bytes take a packet of 254, which occupies 255.
        assert_eq!(packet_filling(256), 254);
        assert_eq!(packet_cost(254), 255);

        for room in 1..=SEGMENT_STEP * 4 {
            let filling = packet_filling(room);

            assert!(packet_cost(filling) <= room, "room {room}");
            assert!(packet_cost(filling + 1) > room, "room {room}");
            assert_eq!(
                packet_cost(filling) == room,
                !room.is_multiple_of(SEGMENT_STEP),
                "room {room}"
            );
        }
    }

    #[cfg(feature = "std")]
    mod std_tests {
        use super::*;
        use std::io::{Cursor, Seek, SeekFrom, Write};

        /// A file in memory, which fails on whichever write, seek or flush a test asks it to.
        struct Scratch {
            bytes: Vec<u8>,
            at: usize,
            writes: usize,
            fail_write: Option<usize>,
            fail_seek: bool,
            fail_flush: bool,
        }

        impl Scratch {
            fn new() -> Self {
                Self {
                    bytes: Vec::new(),
                    at: 0,
                    writes: 0,
                    fail_write: None,
                    fail_seek: false,
                    fail_flush: false,
                }
            }

            fn on_write(at: usize) -> Self {
                Self {
                    fail_write: Some(at),
                    ..Self::new()
                }
            }
        }

        impl Write for Scratch {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let this = self.writes;
                self.writes += 1;

                if self.fail_write == Some(this) {
                    return Err(broken());
                }

                if self.bytes.len() < self.at + buf.len() {
                    self.bytes.resize(self.at + buf.len(), 0);
                }
                self.bytes[self.at..self.at + buf.len()].copy_from_slice(buf);
                self.at += buf.len();

                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                if self.fail_flush {
                    return Err(broken());
                }

                Ok(())
            }
        }

        impl Seek for Scratch {
            fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
                if self.fail_seek {
                    return Err(broken());
                }

                // The writer only ever seeks back to the front of the file, so that is all this
                // answers.
                let SeekFrom::Start(at) = to else {
                    return Err(broken());
                };
                self.at = usize::try_from(at).unwrap();

                Ok(at)
            }
        }

        fn broken() -> std::io::Error {
            std::io::Error::other("the file went away")
        }

        /// What the writer itself made of a call, or nothing when the error came from the file.
        fn refused(error: &WriterIoError) -> Option<WriterError> {
            match error {
                WriterIoError::Writer(error) => Some(*error),
                WriterIoError::Io(_) => None,
            }
        }

        #[test]
        fn writes_a_whole_file_that_reads_back_as_the_golden_file_does() {
            let mut writer =
                write_taf(digest(), AUDIO_ID, tags(), Cursor::new(Vec::new())).unwrap();

            for packet in golden_packets() {
                writer.add_packet(packet, SAMPLES).unwrap();
            }

            let file = writer.finalize().unwrap().into_inner();
            let view = HeaderView::parse(&file[..AUDIO_AT]).unwrap();
            let golden_header = HeaderView::parse(&GOLDEN[..AUDIO_AT]).unwrap();

            // Everything but the header block is the golden file byte for byte; the block itself
            // states the same things, in the bytes this crate writes them in.
            assert_eq!(file.len(), GOLDEN.len());
            assert_eq!(file[AUDIO_AT..], GOLDEN[AUDIO_AT..]);
            assert_eq!(view.sha1(), golden_header.sha1());
            assert_eq!(view.data_length(), golden_header.data_length());
            assert_eq!(view.audio_id(), AUDIO_ID);
        }

        #[test]
        fn reserves_the_header_block_before_it_knows_what_goes_in_it() {
            let mut file = Vec::new();

            {
                let mut writer =
                    write_taf(digest(), AUDIO_ID, tags(), Cursor::new(&mut file)).unwrap();
                writer.add_packet(&packet(700), SAMPLES).unwrap();
            }

            // The block goes in as zeros up front and is filled in by `finalize`; a writer dropped
            // before that leaves it zeroed rather than leaving the audio at the front of the file.
            assert_eq!(file.len(), AUDIO_AT + HEAD_PAGE_LEN + TAGS_PAGE_LEN);
            assert!(file[..AUDIO_AT].iter().all(|&byte| byte == 0));
        }

        #[test]
        fn writes_the_chapters_a_file_was_given() {
            // Written to a plain file rather than a cursor: the header block goes where the writer
            // seeked back to, over the zeros it reserved there.
            let mut writer = write_taf(digest(), AUDIO_ID, tags(), Scratch::new()).unwrap();

            for _ in 0..6 {
                writer.add_packet(&packet(700), SAMPLES).unwrap();
            }
            writer.begin_chapter().unwrap();
            writer.add_packet(&packet(700), SAMPLES).unwrap();

            let file = writer.finalize().unwrap().bytes;
            let view = HeaderView::parse(&file[..AUDIO_AT]).unwrap();

            assert_eq!(
                view.chapter_pages()
                    .map(BlockIndex::get)
                    .collect::<Vec<_>>(),
                [0, 2]
            );
            assert_eq!(
                usize::try_from(view.data_length()),
                Ok(file.len() - AUDIO_AT)
            );
        }

        #[test]
        fn reports_the_file_it_could_not_write_to() {
            // Reserving the header block is the first thing written.
            let reserving = write_taf(digest(), AUDIO_ID, tags(), Scratch::on_write(0));
            assert_eq!(refused(&reserving.err().unwrap()), None);

            // Then the two pages the stream opens with.
            let opening = write_taf(digest(), AUDIO_ID, tags(), Scratch::on_write(1));
            assert_eq!(refused(&opening.err().unwrap()), None);

            // And then whatever page a packet fills up.
            let mut writer = write_taf(digest(), AUDIO_ID, tags(), Scratch::on_write(3)).unwrap();
            let mut hit = None;

            for _ in 0..12 {
                if let Err(error) = writer.add_packet(&packet(700), SAMPLES) {
                    hit = Some(error);
                    break;
                }
            }

            assert_eq!(refused(&hit.unwrap()), None);
        }

        #[test]
        fn reports_the_file_it_could_not_finish() {
            // Seeking back to the front of the file is the only seek the writer makes, and the only
            // one the file written to here answers.
            assert!(Scratch::new().seek(SeekFrom::End(0)).is_err());

            let mut seeking = write_taf(
                digest(),
                AUDIO_ID,
                tags(),
                Scratch {
                    fail_seek: true,
                    ..Scratch::new()
                },
            )
            .unwrap();
            seeking.add_packet(&packet(700), SAMPLES).unwrap();
            assert_eq!(refused(&seeking.finalize().err().unwrap()), None);

            let mut flushing = write_taf(
                digest(),
                AUDIO_ID,
                tags(),
                Scratch {
                    fail_flush: true,
                    ..Scratch::new()
                },
            )
            .unwrap();
            flushing.add_packet(&packet(700), SAMPLES).unwrap();
            assert_eq!(refused(&flushing.finalize().err().unwrap()), None);

            // The header block is written where the file was seeked back to, and that write can
            // fail as well: five pages go in before it — the reserved block, two Opus header pages,
            // one full page and the last one.
            let mut writing = write_taf(digest(), AUDIO_ID, tags(), Scratch::on_write(5)).unwrap();
            for _ in 0..6 {
                writing.add_packet(&packet(700), SAMPLES).unwrap();
            }
            assert_eq!(refused(&writing.finalize().err().unwrap()), None);
        }

        #[test]
        fn reports_what_the_writer_itself_refused() {
            let vendor = String::from_utf8(vec![b'v'; 500]).unwrap();
            let tags_too_long = write_taf(
                digest(),
                AUDIO_ID,
                Tags::new(&vendor, &[]),
                Cursor::new(Vec::new()),
            );

            assert_eq!(
                refused(&tags_too_long.err().unwrap()),
                Some(WriterError::Build(BuildError::TagsTooLong))
            );

            let mut writer =
                write_taf(digest(), AUDIO_ID, tags(), Cursor::new(Vec::new())).unwrap();

            assert_eq!(
                writer
                    .add_packet(&packet(3544), SAMPLES)
                    .err()
                    .as_ref()
                    .and_then(refused),
                Some(WriterError::PacketTooLarge)
            );
            assert!(writer.begin_chapter().is_ok());

            // A packet that cannot be padded is refused where the page it ends is closed, which is
            // when the file is finished.
            writer.add_packet(&[TOC | 1, 1, 2, 3], SAMPLES).unwrap();

            assert_eq!(
                refused(&writer.finalize().err().unwrap()),
                Some(WriterError::Pad(PadError::MalformedToc))
            );
        }

        #[test]
        fn every_error_says_what_went_wrong_and_what_it_wraps() {
            use std::error::Error;

            let refused = WriterIoError::from(WriterError::PacketTooLarge);
            let broke = WriterIoError::from(broken());

            assert_eq!(
                std::string::ToString::to_string(&refused),
                "an Opus packet is longer than a TAF page carries"
            );
            assert_eq!(
                std::string::ToString::to_string(&broke),
                "the file went away"
            );
            assert!(refused.source().is_some());
            assert!(broke.source().is_some());
        }
    }
}
