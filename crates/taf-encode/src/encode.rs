//! Where samples become a file: libopus at the settings a Toniebox expects, and `taf`'s writer
//! behind it.
//!
//! # The settings, and why they are not choices
//!
//! 48 kHz, two channels, 60 ms to a packet, VBR at 96 kbit/s. Every one of them is what teddycloud
//! encodes with — `FORMAT.md` in the `taf` crate cites the line of it each comes from — and a file
//! that states anything else is a file a box was never asked to play. So none of them is a knob
//! here either.
//!
//! # A frame is 2880 samples of each channel, whatever is left of one
//!
//! An Opus packet of a TAF carries 60 ms, which is 2880 frames at 48 kHz, and the granule
//! positions of the file's pages count in them. Audio does not arrive in multiples of that — a
//! chapter begins where its audio begins, and a recording ends where it ends — so what is left of
//! a frame at a chapter boundary and at the end of the conversion is filled out with silence and
//! encoded like any other.
//!
//! Filling out rather than rounding the boundary is what keeps a chapter's audio whole: the
//! chapter begins the packet behind the fill, so no frame carries the end of one chapter and the
//! start of the next, and nothing of either is lost or moved into the other. What it costs is
//! under 60 ms of silence at the end of the chapter in front of the boundary, once per chapter —
//! which is silence in the place a book already has some, since a chapter is where a pause goes.
//!
//! # Counting the bytes the writer writes
//!
//! `taf`'s writer states where a chapter begins in blocks of the audio region, and states it in the
//! header block it hands over once the file is finished. A conversion reports its chapters while
//! the file is still being written, so the bytes going into the file are counted on their way past
//! and the block the next page starts is what they come to — the same arithmetic the writer does
//! for the list it will state.

use std::cell::Cell;
use std::io::{self, Seek, SeekFrom, Write};
use std::rc::Rc;

use opus::{Application, Bitrate, Channels, Encoder, FrameSize};
use taf::digest::Sha1;
use taf::id::{AudioId, BlockIndex};
use taf::writer::{write_taf, StdTafWriter, Tags};

use crate::convert::ConvertError;

/// The rate a TAF is encoded at, which is the rate Opus is defined at.
const RATE: u32 = 48_000;

/// The samples of one channel one Opus packet carries: 60 ms at [`RATE`], and what the granule
/// position of every page of the file advances by.
pub(crate) const FRAME: u32 = 2_880;

/// The same frame as the samples it is handed over as: [`FRAME`] of each of the two channels a TAF
/// carries, interleaved.
const FRAME_SAMPLES: usize = FRAME as usize * 2;

/// What the encoder is asked to spend on a second of audio, in bits.
const BITRATE: i32 = 96_000;

/// The longest packet the encoder may hand back: what the first audio page of a file holds, which
/// is the shortest page a TAF has — every page behind it holds 4053. A 60 ms frame at the bitrate
/// above is around 720 bytes, so this is a bound and never a target.
const MAX_PACKET: usize = 3_543;

/// The bytes one block of a TAF occupies: the same 4096 as [`taf::header::BLOCK_LEN`], counted the
/// way the bytes written to the file are.
const BLOCK: u64 = 4_096;

/// Who wrote the stream, as its `OpusTags` packet states it — teddycloud writes `teddyCloud` here
/// and this is the same thing said about this converter.
const VENDOR: &str = "taffle";

/// The one comment behind it: `ENCODER` is the Vorbis comment field for the software a stream was
/// encoded with, and the version is this crate's own.
const COMMENT: &str = concat!("ENCODER=taffle ", env!("CARGO_PKG_VERSION"));

/// The SHA-1 a TAF's header states, over the `sha1` crate's implementation of it.
///
/// This is the consumer side of [`taf::digest::Sha1`]: the format crate states what a digest has to
/// do and implements none, and this is where a host build says which one it is.
struct Digest(sha1::Sha1);

impl Sha1 for Digest {
    fn update(&mut self, data: &[u8]) {
        sha1::Digest::update(&mut self.0, data);
    }

    fn finalize(self) -> [u8; 20] {
        sha1::Digest::finalize(self.0).into()
    }
}

/// The output of a conversion, with a running count of the bytes that have gone into it.
///
/// Everything the writer writes goes through here, in the order it writes it: the block it reserves
/// for the header, the two pages the Opus stream opens with, and then a page at a time. So while a
/// file is being written, what has been counted is the header block and the audio region so far —
/// which is what says where the next page falls. Once the file is finished the count means nothing
/// more, since finishing seeks back to fill the header block in.
struct Counted<W> {
    out: W,
    written: Rc<Cell<u64>>,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.out.write(buf)?;
        let counted = u64::try_from(written).unwrap_or(u64::MAX);
        self.written.set(self.written.get().saturating_add(counted));

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl<W: Seek> Seek for Counted<W> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.out.seek(to)
    }
}

/// A TAF being written out of 48 kHz stereo samples: the Opus encoder, and `taf`'s writer behind
/// it.
///
/// [`push`](Self::push) takes the samples as they come and encodes every whole frame they complete,
/// [`begin_chapter`](Self::begin_chapter) starts a chapter at the next block, and
/// [`finish`](Self::finish) writes what is left and closes the file.
pub(crate) struct TafEncoder<W: Write + Seek> {
    encoder: Encoder,
    writer: StdTafWriter<Digest, Counted<W>>,
    /// The samples of the frame being filled, interleaved and fewer than a whole frame's.
    pending: Vec<i16>,
    /// The frames of one channel encoded so far, the silence a frame was filled out with counted
    /// in — which is where the audio of the next chapter begins.
    frames: u64,
    /// The bytes written to the file so far, which is what the chapter blocks are counted from.
    written: Rc<Cell<u64>>,
}

impl<W: Write + Seek> TafEncoder<W> {
    /// Opens a file of `audio_id` in `out`, and an encoder at the settings a TAF states.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Encode`] if libopus refuses the settings, and [`ConvertError::Io`] or
    /// [`ConvertError::Taf`] if the file could not be opened the way a TAF opens.
    pub(crate) fn new(audio_id: AudioId, out: W) -> Result<Self, ConvertError> {
        let mut encoder = Encoder::new(RATE, Channels::Stereo, Application::Audio)?;
        encoder.set_vbr(true)?;
        encoder.set_bitrate(Bitrate::Bits(BITRATE))?;
        encoder.set_expert_frame_duration(FrameSize::Ms60)?;

        let written = Rc::new(Cell::new(0));
        let digest = Digest(<sha1::Sha1 as sha1::Digest>::new());
        let file = Counted {
            out,
            written: Rc::clone(&written),
        };
        let writer = write_taf(digest, audio_id, Tags::new(VENDOR, &[COMMENT]), file)?;

        Ok(Self {
            encoder,
            writer,
            pending: Vec::with_capacity(FRAME_SAMPLES),
            frames: 0,
            written,
        })
    }

    /// Encodes every whole 60 ms frame that `block` completes, and keeps what is left of one.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Encode`] if libopus refuses a frame, and [`ConvertError::Io`] or
    /// [`ConvertError::Taf`] if a page could not be written.
    pub(crate) fn push(&mut self, block: &[i16]) -> Result<(), ConvertError> {
        let mut rest = block;

        // The frame in hand is encoded the moment it is full, so there is room for a sample at the
        // top of every turn and every turn takes at least one.
        while !rest.is_empty() {
            let room = FRAME_SAMPLES.saturating_sub(self.pending.len());
            let (taken, left) = rest.split_at_checked(room).unwrap_or((rest, &[]));
            self.pending.extend_from_slice(taken);
            rest = left;

            if self.pending.len() >= FRAME_SAMPLES {
                self.encode_frame()?;
            }
        }

        Ok(())
    }

    /// Starts a chapter at the block behind the audio pushed so far.
    ///
    /// What is left of the frame in hand is filled out with silence and encoded first, so the
    /// chapter's own audio begins a packet — and a page, and a block — of its own.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Encode`] if libopus refuses that frame, and [`ConvertError::Io`] or
    /// [`ConvertError::Taf`] if the page it closes could not be written.
    pub(crate) fn begin_chapter(&mut self) -> Result<(), ConvertError> {
        if !self.pending.is_empty() {
            self.encode_frame()?;
        }
        self.writer.begin_chapter()?;

        Ok(())
    }

    /// The block the audio behind the last chapter begun starts at.
    pub(crate) fn block(&self) -> BlockIndex {
        // A file opens with the header block, which is not part of the audio region the chapter
        // blocks are counted in.
        let audio = self.written.get().saturating_sub(BLOCK);

        BlockIndex::new(u32::try_from(audio / BLOCK).unwrap_or(u32::MAX))
    }

    /// The frames of one channel the file carries so far, the silence frames were filled out with
    /// counted in.
    pub(crate) fn frames(&self) -> u64 {
        self.frames
    }

    /// Writes what is left, finishes the file, and states how many frames it came to.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Encode`] if libopus refuses the last frame, and [`ConvertError::Io`] or
    /// [`ConvertError::Taf`] if the last pages or the header block could not be written.
    pub(crate) fn finish(mut self) -> Result<u64, ConvertError> {
        // A TAF's first block holds the two Opus header pages and an audio page, so every file has
        // a packet in it: a conversion that came out with no audio at all is the one 60 ms frame of
        // silence that makes what was written a file.
        if !self.pending.is_empty() || self.frames == 0 {
            self.encode_frame()?;
        }

        let Self { writer, frames, .. } = self;
        writer.finalize()?;

        Ok(frames)
    }

    /// Encodes the frame in hand, filled out with silence where it is short of a whole one, and
    /// puts the packet in the file.
    fn encode_frame(&mut self) -> Result<(), ConvertError> {
        self.pending.resize(FRAME_SAMPLES, 0);
        let packet = self.encoder.encode_vec(&self.pending, MAX_PACKET)?;
        self.writer.add_packet(&packet, FRAME)?;

        self.pending.clear();
        self.frames = self.frames.saturating_add(u64::from(FRAME));

        Ok(())
    }
}
