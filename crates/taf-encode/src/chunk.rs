//! Where the audio of a conversion is cut into chunks: pieces that can be encoded on their own and
//! written back in the order they were cut.
//!
//! # A cut lands on the packet grid, because everything else does
//!
//! An Opus packet is the smallest thing a TAF has: a page carries whole packets, a granule position
//! counts them, and a chapter begins at one. So a chunk is a whole number of packets and nothing
//! else — the piece in front of a cut is filled out with silence the way a chapter boundary is, and
//! the piece behind it starts a packet of its own. Cutting anywhere else would mean two chunks
//! sharing a packet, which is a packet neither of them can encode.
//!
//! # A cut moves to the quietest packet in reach
//!
//! Two chunks encoded apart meet at a seam, and an encoder that starts at a seam does not know the
//! audio in front of it. What that costs depends on what is at the seam: in the middle of a spoken
//! word it is audible, in a pause there is nothing to get wrong. So a cut aims at
//! [`TARGET_PACKETS`] and then looks [`SNAP_PACKETS`] either way for the quietest packet of the
//! window and goes in front of it — which in a book lands in the pause between two sentences,
//! since that is where the quiet is.
//!
//! # A warm-up is the tail of the stream in front, padding and all
//!
//! An Opus encoder carries state, so the first packets it hands back after a fresh start differ
//! from what one encoder running through the whole book would have produced there. The fix is to
//! hand every chunk the last [`WARMUP_PACKETS`] packets of the audio in front of it, encoded and
//! thrown away: by the time the chunk's own audio arrives, the encoder is in the state it would
//! have been in. Those packets are taken from the *padded* stream — the silence that fills a
//! chapter boundary out is audio the continuous encoder saw, and a warm-up that skipped it would
//! warm up on a stream that never existed.
//!
//! # Which is why none of this asks how many workers there are
//!
//! Where the cuts fall is a function of the audio alone: the same samples make the same chunks,
//! carrying the same warm-ups, in the same order — whether one worker encodes them or eight, and
//! whatever order the workers happen to finish in. A conversion's bytes are the grid's, and the
//! grid is the audio's.

use crate::encode::FRAME_SAMPLES;

/// How many packets a chunk aims for: ~20 s, long enough that a warm-up is a rounding error.
pub(crate) const TARGET_PACKETS: usize = 333;

/// How far a cut may move to land on a pause: ~2 s either way.
pub(crate) const SNAP_PACKETS: usize = 33;

/// How many packets warm a fresh encoder up: against the libopus this was measured on, 8 packets
/// still moved the bytes when the warm-up grew and 12 landed byte-identical — by then the state a
/// fresh encoder starts from has leaked back out of it. How exactly the bytes settle is that
/// build's own affair; what holds across libopus builds is that whatever difference a longer
/// warm-up still makes sits far below hearing, and the convergence test in `encode.rs` pins that
/// bound.
pub(crate) const WARMUP_PACKETS: usize = 12;

/// One chunk of the conversion, as the encoding side is handed it.
pub(crate) struct Job {
    /// Where the job stands in the conversion; packets come back in this order.
    pub index: usize,
    /// Whole packets in front of the chunk, encoded and thrown away to warm the encoder up.
    pub warmup: Vec<i16>,
    /// The chunk itself: interleaved stereo, padded with silence to whole packets.
    pub pcm: Vec<i16>,
    /// The chapters that begin where this chunk begins, each under its title.
    pub chapters: Vec<Option<String>>,
}

/// The audio of a conversion on its way past, cut into jobs as it goes.
///
/// The blocks a reading hands over are one stream here, and a chunk is taken off the front of it
/// whenever enough of it has arrived — so a conversion holds a chunk and change rather than the
/// whole book, however long the book is.
pub(crate) struct Chunker {
    /// The samples that have arrived and not been cut off yet: whole packets, except that the last
    /// of them is whatever arrived of one.
    buffer: Vec<i16>,
    /// The chapters that begin where the *next* job begins, in the order they were begun.
    pending: Vec<Option<String>>,
    /// The last [`WARMUP_PACKETS`] packets of the padded stream already handed out, which is what
    /// the next job warms its encoder up on.
    tail: Vec<i16>,
    /// What the next job is called, counted from the start of the conversion.
    index: usize,
}

impl Chunker {
    /// A chunker at the start of a stream, with nothing cut and nothing buffered.
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            pending: Vec::new(),
            tail: Vec::new(),
            index: 0,
        }
    }

    /// Takes the next block; hands a job back when the buffer crossed the cut line.
    ///
    /// The line is [`TARGET_PACKETS`] plus the room a cut needs to move backwards, so that the
    /// whole window a cut may land in is in hand by the time the landing place is picked.
    pub(crate) fn push_block(&mut self, block: Vec<i16>) -> Option<Job> {
        self.buffer.extend(block);
        if self.buffer.len() < (TARGET_PACKETS + SNAP_PACKETS) * FRAME_SAMPLES {
            return None;
        }

        let at = quietest(&self.buffer)?;
        let rest = self.buffer.split_off(at * FRAME_SAMPLES);
        let pcm = std::mem::replace(&mut self.buffer, rest);

        Some(self.emit(pcm))
    }

    /// A chapter begins here: what is buffered becomes a job, and the title waits for the next.
    ///
    /// The job that comes of it carries the chapters that were already pending — those are the ones
    /// that began where *its* audio begins. Where nothing is buffered there is no job to hand back
    /// and the titles pile up, so several chapters in a row begin at one chunk rather than at
    /// chunks with no audio in them.
    pub(crate) fn begin_chapter(&mut self, title: Option<String>) -> Option<Job> {
        let job = if self.buffer.is_empty() {
            None
        } else {
            let pcm = std::mem::take(&mut self.buffer);

            Some(self.emit(pcm))
        };
        self.pending.push(title);

        job
    }

    /// The end of the stream: whatever is left, as the last job.
    ///
    /// Titles that pend with no audio behind them are chapters at the very end of the book, and
    /// they are handed over as a job with no audio in it — the chapters still begin, and they begin
    /// where the file ends.
    pub(crate) fn finish(&mut self) -> Option<Job> {
        if self.buffer.is_empty() && self.pending.is_empty() {
            return None;
        }
        let pcm = std::mem::take(&mut self.buffer);

        Some(self.emit(pcm))
    }

    /// Makes `pcm` the next job, and remembers its end as the warm-up of the job behind it.
    ///
    /// The tail is taken from the padded audio and across job boundaries, so a run of chunks too
    /// short to fill a warm-up on their own still hands the full one over: what warms an encoder up
    /// is the stream in front of the chunk, not the chunk in front of it.
    fn emit(&mut self, mut pcm: Vec<i16>) -> Job {
        pad(&mut pcm);
        let job = Job {
            index: self.index,
            warmup: self.tail.clone(),
            pcm,
            chapters: std::mem::take(&mut self.pending),
        };
        self.index += 1;

        self.tail.extend_from_slice(&job.pcm);
        let over = self
            .tail
            .len()
            .saturating_sub(WARMUP_PACKETS * FRAME_SAMPLES);
        self.tail.drain(..over);

        job
    }
}

/// The packet a cut goes in front of: the quietest one of the window a cut may move in, and the
/// earliest of those where several are equally quiet.
///
/// Ties go to the earliest because a stretch of digital silence is one pause rather than several,
/// and its start is where the pause begins. [`None`] means the window is not in the buffer yet,
/// which is no place to cut.
fn quietest(buffer: &[i16]) -> Option<usize> {
    buffer
        .chunks_exact(FRAME_SAMPLES)
        .enumerate()
        .skip(TARGET_PACKETS - SNAP_PACKETS)
        .take(2 * SNAP_PACKETS)
        .min_by_key(|(_, packet)| peak(packet))
        .map(|(at, _)| at)
}

/// How loud a packet is: the sample of it furthest from silence.
///
/// The distance is counted in [`i32`], where the one sample an [`i16`] has no room to negate has
/// room — a packet holding [`i16::MIN`] is the loudest one there is and never the quietest.
fn peak(packet: &[i16]) -> i32 {
    packet
        .iter()
        .fold(0, |peak, sample| peak.max(i32::from(*sample).abs()))
}

/// Fills `pcm` out to whole packets with silence, the way a chapter boundary is filled out.
fn pad(pcm: &mut Vec<i16>) {
    let over = pcm.len() % FRAME_SAMPLES;
    if over > 0 {
        pcm.resize(pcm.len() + FRAME_SAMPLES - over, 0);
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
    use super::{Chunker, FRAME_SAMPLES, SNAP_PACKETS, TARGET_PACKETS, WARMUP_PACKETS};

    /// A block of `packets` whole packets, every sample at `level`.
    fn level_packets(packets: usize, level: i16) -> Vec<i16> {
        vec![level; packets * FRAME_SAMPLES]
    }

    #[test]
    fn a_stream_shorter_than_a_chunk_is_one_job_at_the_end() {
        let mut chunker = Chunker::new();

        assert!(chunker.push_block(level_packets(10, 1_000)).is_none());
        let job = chunker.finish().unwrap();

        assert_eq!(job.index, 0);
        assert_eq!(job.pcm.len(), 10 * FRAME_SAMPLES);
        assert!(job.warmup.is_empty());
        assert!(chunker.finish().is_none());
    }

    #[test]
    fn the_cut_lands_in_front_of_the_quietest_packet_of_the_window() {
        let mut chunker = Chunker::new();
        // Loud everywhere, one silent packet inside the snap window.
        let quiet_at = TARGET_PACKETS + 7;
        let mut samples = level_packets(TARGET_PACKETS + SNAP_PACKETS, 20_000);
        let start = quiet_at * FRAME_SAMPLES;
        samples[start..start + FRAME_SAMPLES].fill(0);

        let job = chunker.push_block(samples).unwrap();

        assert_eq!(job.pcm.len(), quiet_at * FRAME_SAMPLES);
    }

    #[test]
    fn a_tie_between_two_equally_quiet_packets_goes_to_the_earlier_one() {
        let mut chunker = Chunker::new();
        let samples = level_packets(TARGET_PACKETS + SNAP_PACKETS, 20_000);

        let job = chunker.push_block(samples).unwrap();

        // Every packet is equally loud, so the earliest packet of the window wins.
        assert_eq!(
            job.pcm.len(),
            (TARGET_PACKETS - SNAP_PACKETS) * FRAME_SAMPLES
        );
    }

    #[test]
    fn a_chapter_cuts_immediately_and_titles_the_next_job() {
        let mut chunker = Chunker::new();

        let first = chunker.push_block(vec![1_000; FRAME_SAMPLES + 4]);
        assert!(first.is_none());
        let cut = chunker.begin_chapter(Some(String::from("Two"))).unwrap();
        let last = chunker.finish().unwrap();

        // The chunk in front of the chapter is padded out to whole packets with silence.
        assert_eq!(cut.pcm.len(), 2 * FRAME_SAMPLES);
        assert_eq!(
            &cut.pcm[FRAME_SAMPLES + 4..],
            vec![0; FRAME_SAMPLES - 4].as_slice()
        );
        assert!(cut.chapters.is_empty());
        // The chapter at the very end brings no audio of its own, not even a silent packet.
        assert_eq!(last.chapters, [Some(String::from("Two"))]);
        assert!(last.pcm.is_empty());
    }

    #[test]
    fn chapters_with_no_audio_between_them_pile_onto_one_job() {
        let mut chunker = Chunker::new();

        assert!(chunker.begin_chapter(None).is_none());
        assert!(chunker.begin_chapter(Some(String::from("Empty"))).is_none());
        assert!(chunker.push_block(level_packets(1, 500)).is_none());
        let job = chunker.finish().unwrap();

        assert_eq!(job.chapters, [None, Some(String::from("Empty"))]);
    }

    #[test]
    fn the_warmup_is_the_tail_of_the_padded_stream_in_front() {
        let mut chunker = Chunker::new();

        let first = chunker
            .push_block(level_packets(TARGET_PACKETS + SNAP_PACKETS, 20_000))
            .unwrap();
        let second = chunker.finish().unwrap();

        assert_eq!(second.index, 1);
        assert_eq!(second.warmup.len(), WARMUP_PACKETS * FRAME_SAMPLES);
        let tail = &first.pcm[first.pcm.len() - WARMUP_PACKETS * FRAME_SAMPLES..];
        assert_eq!(second.warmup, tail);
    }

    #[test]
    fn a_warmup_never_reaches_across_more_audio_than_there_was() {
        let mut chunker = Chunker::new();

        let first = chunker.push_block(vec![1_000; FRAME_SAMPLES / 2]);
        assert!(first.is_none());
        let cut = chunker.begin_chapter(None).unwrap();
        assert!(chunker.push_block(level_packets(1, 2_000)).is_none());
        let last = chunker.finish().unwrap();

        // One padded packet existed in front, so one padded packet is the whole warm-up.
        assert_eq!(cut.pcm.len(), FRAME_SAMPLES);
        assert_eq!(last.warmup.len(), FRAME_SAMPLES);
    }

    #[test]
    fn a_warmup_reaches_back_across_several_chunks_too_short_to_fill_one() {
        let mut chunker = Chunker::new();

        // A chapter every packet: no chunk on its own carries a whole warm-up.
        for level in [1_000, 2_000, 3_000] {
            assert!(chunker.push_block(level_packets(1, level)).is_none());
            assert!(chunker.begin_chapter(None).is_some());
        }
        let last = chunker.finish().unwrap();

        // So the warm-up is the three of them in a row, oldest packet first.
        assert_eq!(last.warmup.len(), 3 * FRAME_SAMPLES);
        assert_eq!(last.warmup[0], 1_000);
        assert_eq!(last.warmup[FRAME_SAMPLES], 2_000);
        assert_eq!(last.warmup[2 * FRAME_SAMPLES], 3_000);
    }

    #[test]
    fn an_empty_stream_is_no_job_at_all() {
        let mut chunker = Chunker::new();

        assert!(chunker.finish().is_none());
    }

    #[test]
    fn a_chunk_worth_of_audio_is_not_cut_until_the_whole_window_is_in_hand() {
        let mut chunker = Chunker::new();

        // A cut may move backwards as far as it may move forwards, so what a chunk aims for is
        // not on its own enough to pick a place: the audio behind it decides too.
        let held = chunker.push_block(level_packets(TARGET_PACKETS, 20_000));
        let job = chunker.finish().unwrap();

        assert!(held.is_none());
        assert_eq!(job.pcm.len(), TARGET_PACKETS * FRAME_SAMPLES);
    }

    #[test]
    fn the_loudest_sample_there_is_never_reads_as_a_pause() {
        let mut chunker = Chunker::new();
        // A distance from silence counted in `i16` has no room for `i16::MIN`, and the packet
        // holding the loudest sample there is would come out the quietest of the window.
        let loud_at = TARGET_PACKETS + 7;
        let mut samples = level_packets(TARGET_PACKETS + SNAP_PACKETS, 20_000);
        let start = loud_at * FRAME_SAMPLES;
        samples[start..start + FRAME_SAMPLES].fill(i16::MIN);

        let job = chunker.push_block(samples).unwrap();

        // So the cut goes where it would have gone anyway: the earliest of the equally loud ones.
        assert_eq!(
            job.pcm.len(),
            (TARGET_PACKETS - SNAP_PACKETS) * FRAME_SAMPLES
        );
    }
}
