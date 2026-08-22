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
//! # The encoder and the writer come apart
//!
//! Samples become packets in one place and packets become a file in another: [`encode_job`] takes a
//! chunk of audio and hands its packets back, and [`PacketSink`] takes packets and writes the file
//! they make. Nothing passes between them but the packets, so the two ends need not run in the same
//! place or at the same time — which is what lets a conversion encode its chunks on every core it
//! has and write them out on one.
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

use crate::chunk::Job;
use crate::convert::ConvertError;

/// The rate a TAF is encoded at, which is the rate Opus is defined at.
const RATE: u32 = 48_000;

/// The samples of one channel one Opus packet carries: 60 ms at [`RATE`], and what the granule
/// position of every page of the file advances by.
pub(crate) const FRAME: u32 = 2_880;

/// The same frame as the samples it is handed over as: [`FRAME`] of each of the two channels a TAF
/// carries, interleaved.
pub(crate) const FRAME_SAMPLES: usize = FRAME as usize * 2;

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

/// An encoder at the settings a TAF states. See the module header: none of them is a knob.
fn configured() -> Result<Encoder, opus::Error> {
    let mut encoder = Encoder::new(RATE, Channels::Stereo, Application::Audio)?;
    encoder.set_vbr(true)?;
    encoder.set_bitrate(Bitrate::Bits(BITRATE))?;
    encoder.set_expert_frame_duration(FrameSize::Ms60)?;

    Ok(encoder)
}

/// Encodes one job with a fresh encoder: the warm-up packets are encoded and thrown away, the
/// chunk's packets come back in order.
///
/// A fresh encoder per job is what makes a job's bytes a function of the job alone — nothing here
/// carries over from the job in front of it or depends on which worker took it.
///
/// # Errors
///
/// [`opus::Error`] if libopus refuses the settings or a frame.
pub(crate) fn encode_job(job: &Job) -> Result<Vec<Vec<u8>>, opus::Error> {
    let mut encoder = configured()?;
    let mut packet = vec![0_u8; MAX_PACKET];

    // The warm-up converges the encoder's leaky memory toward what a continuous encoder's would
    // have been here; its packets are practice and nobody hears them.
    for frame in job.warmup.chunks_exact(FRAME_SAMPLES) {
        let _ = encoder.encode(frame, &mut packet)?;
    }

    job.pcm
        .chunks_exact(FRAME_SAMPLES)
        .map(|frame| {
            let len = encoder.encode(frame, &mut packet)?;
            // The length libopus states is at most the buffer it was handed, so this never comes
            // up empty for a packet that was encoded.
            Ok(packet.get(..len).unwrap_or_default().to_vec())
        })
        .collect()
}

/// A TAF being written out of packets that were encoded somewhere else.
///
/// Packets go in whole, in the order the file carries them, and the bytes on their way past say
/// where a chapter's block falls. A file whose audio came to nothing is the caller's business here,
/// since making the one packet of silence such a file needs takes an encoder and there is none
/// behind this.
pub(crate) struct PacketSink<W: Write + Seek> {
    writer: StdTafWriter<Digest, Counted<W>>,
    /// The frames of one channel written so far, which is where the audio of the next chapter
    /// begins.
    frames: u64,
    /// The bytes written to the file so far, which is what the chapter blocks are counted from.
    written: Rc<Cell<u64>>,
}

impl<W: Write + Seek> PacketSink<W> {
    /// Opens a file of `audio_id` in `out`.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Io`] or [`ConvertError::Taf`] if the file could not be opened the way a TAF
    /// opens.
    pub(crate) fn new(audio_id: AudioId, out: W) -> Result<Self, ConvertError> {
        let written = Rc::new(Cell::new(0));
        let digest = Digest(<sha1::Sha1 as sha1::Digest>::new());
        let file = Counted {
            out,
            written: Rc::clone(&written),
        };
        let writer = write_taf(digest, audio_id, Tags::new(VENDOR, &[COMMENT]), file)?;

        Ok(Self {
            writer,
            frames: 0,
            written,
        })
    }

    /// Starts a chapter at the block behind the packets written so far.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Io`] or [`ConvertError::Taf`] if the page it closes could not be written.
    pub(crate) fn begin_chapter(&mut self) -> Result<(), ConvertError> {
        self.writer.begin_chapter()?;

        Ok(())
    }

    /// Puts one whole 60 ms packet in the file.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Io`] or [`ConvertError::Taf`] if the page it fills could not be written.
    pub(crate) fn push_packet(&mut self, packet: &[u8]) -> Result<(), ConvertError> {
        self.writer.add_packet(packet, FRAME)?;
        self.frames = self.frames.saturating_add(u64::from(FRAME));

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

    /// Finishes the file, and states how many frames it came to.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Io`] or [`ConvertError::Taf`] if the last pages or the header block could
    /// not be written.
    pub(crate) fn finish(self) -> Result<u64, ConvertError> {
        let Self { writer, frames, .. } = self;
        writer.finalize()?;

        Ok(frames)
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
    use std::io::Cursor;

    use taf::id::AudioId;

    use super::{encode_job, PacketSink, FRAME, FRAME_SAMPLES, MAX_PACKET};
    use crate::chunk::Job;

    /// A job of `packets` whole packets of a quiet ramp, warmed up by `warmup` packets of the same.
    fn job_of(warmup: usize, packets: usize) -> Job {
        let ramp = |len: usize| -> Vec<i16> {
            (0..len)
                .map(|at| (i16::try_from(at % 97).unwrap() - 48) * 200)
                .collect()
        };

        Job {
            index: 0,
            warmup: ramp(warmup * FRAME_SAMPLES),
            pcm: ramp(packets * FRAME_SAMPLES),
            chapters: Vec::new(),
        }
    }

    #[test]
    fn a_job_comes_back_as_exactly_its_own_packets() {
        let batch = encode_job(&job_of(4, 10)).unwrap();

        assert_eq!(batch.len(), 10, "the warm-up packets are not in the answer");
        assert!(batch.iter().all(|packet| !packet.is_empty()));
        assert!(batch.iter().all(|packet| packet.len() <= MAX_PACKET));
    }

    #[test]
    fn the_same_job_encodes_to_the_same_bytes() {
        let job = job_of(4, 10);

        assert_eq!(encode_job(&job).unwrap(), encode_job(&job).unwrap());
    }

    #[test]
    fn the_packets_a_job_encodes_are_opus_a_decoder_reads() {
        let batch = encode_job(&job_of(0, 3)).unwrap();
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut samples = vec![0_i16; FRAME_SAMPLES];

        for packet in &batch {
            let frames = decoder.decode(packet, &mut samples, false).unwrap();
            assert_eq!(
                frames, FRAME as usize,
                "every packet carries one whole frame"
            );
        }
    }

    #[test]
    fn a_sink_counts_blocks_and_frames_the_way_the_file_does() {
        let mut sink = PacketSink::new(AudioId::new(1), Cursor::new(Vec::new())).unwrap();

        assert_eq!(sink.frames(), 0);
        assert_eq!(sink.block().get(), 0);
        sink.push_packet(&[200; 400]).unwrap();
        assert_eq!(sink.frames(), u64::from(FRAME));
        // A packet is in an open page until a chapter closes it, and the block behind that page is
        // the one the chapter's own audio begins at.
        assert_eq!(sink.block().get(), 0);
        sink.begin_chapter().unwrap();
        assert_eq!(sink.block().get(), 1);
        let finished = sink.finish().unwrap();

        assert_eq!(finished, u64::from(FRAME));
    }
}
