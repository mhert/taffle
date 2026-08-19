//! The engine end to end: audio files in, a TAF out — decoded, brought to 48 kHz, silence-processed,
//! Opus-encoded and written the way the format states it.
//!
//! Every conversion here goes through [`validated`], which reads the file back the way anything
//! that reads a TAF reads one: `taf`'s own validator over the blocks of the audio region, with the
//! SHA-1 the header states checked against the bytes that were written. So nothing below says
//! anything about a conversion that is not first a file which holds up.
//!
//! What the audio itself came out as is read the same way: [`decoded`] hands the file's own Opus
//! packets to libopus and gets the samples back, which is what a test about silence and tone can
//! be exact about.

// Every cast below is on a count a test states or on a wave bounded by the peak it was scaled
// with, and every index is into a stream a test built or the conversion just handed out.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

// Shared with the decode and pcm tests: the WAV builder, the m4b and its cover.
#[allow(dead_code)]
mod fixtures;

use std::error::Error;
use std::f64::consts::TAU;
use std::io::{self, Cursor, Seek, SeekFrom, Write};
use std::time::Duration;

use taf::digest::Sha1;
use taf::header::{HeaderView, BLOCK_LEN};
use taf::id::AudioId;
use taf::ogg::{PageView, OPUS_PRE_SKIP};
use taf::reader::{Summary, Validator};
use taf_encode::{
    convert, ChapterMode, Conversion, ConversionReport, ConvertError, Input, Progress, SilenceOpts,
};

/// The rate everything a TAF carries is counted at.
const RATE: u32 = 48_000;

/// The samples of one channel one Opus packet of a TAF carries: 60 ms at 48 kHz.
const FRAME: u64 = 2_880;

/// How long one of those packets plays.
const FRAME_TIME: Duration = Duration::from_millis(60);

/// The audio id every conversion here is asked for.
const AUDIO_ID: AudioId = AudioId::new(0x2508_1918);

/// What the tones built here peak at, well clear of the level a frame counts as silence below.
const PEAK: i16 = 12_000;

/// The frequency of those tones, in Hz.
const TONE_HZ: f64 = 440.0;

/// What a stretch of decoded silence stays under: digital silence through the codec comes back
/// under a hundredth of the tone around it.
const QUIET: i32 = PEAK as i32 / 100;

/// What a stretch of decoded tone reaches: the codec keeps a sine well inside this.
const LOUD: i32 = PEAK as i32 * 3 / 4;

/// What a short stretch of silence between two stretches of tone comes back under: a twentieth of
/// the tone, where the 20 ms one below measured 293 of 12 000. A lossy codec fits 20 ms of nothing
/// between two of something with a tail either side of it, and [`QUIET`] is not what that is.
const FADED: i32 = PEAK as i32 / 20;

/// One input of a conversion, out of bytes a test built.
fn input(bytes: Vec<u8>, name: &str) -> Input {
    Input {
        reader: Box::new(Cursor::new(bytes)),
        name: name.to_owned(),
    }
}

/// `frames` frames of a 440 Hz tone at 48 kHz, interleaved over two channels.
fn tone(frames: u64) -> Vec<i16> {
    let mut samples = Vec::with_capacity(frames as usize * 2);
    for frame in 0..frames {
        let value = (TAU * TONE_HZ * frame as f64 / f64::from(RATE)).sin();
        let sample = (value * f64::from(PEAK)).round() as i16;
        samples.push(sample);
        samples.push(sample);
    }

    samples
}

/// `frames` frames of digital silence at 48 kHz, interleaved over two channels.
fn silence(frames: u64) -> Vec<i16> {
    vec![0; frames as usize * 2]
}

/// A WAV of `samples` at the 48 kHz stereo a TAF carries, so that nothing resamples on the way
/// through and what a test states about a frame is the frame the encoder sees.
fn wav(samples: &[i16]) -> Vec<u8> {
    fixtures::wav_of(RATE, 2, samples)
}

/// A WAV of `frames` frames of tone.
fn tone_wav(frames: u64) -> Vec<u8> {
    wav(&tone(frames))
}

/// An output with room for so many bytes and no more: everything that would go past that is
/// refused, the way a disk with nothing left on it refuses one.
struct Full {
    taken: usize,
    room: usize,
}

impl Full {
    /// An output of `room` bytes.
    fn with(room: usize) -> Self {
        Self { taken: 0, room }
    }
}

impl Write for Full {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.taken + buf.len() > self.room {
            return Err(io::Error::other("no room left"));
        }
        self.taken += buf.len();

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Full {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Ok(0)
    }
}

/// The digest a TAF is hashed with, over the `sha1` crate's implementation of it.
struct Digest(sha1::Sha1);

impl Sha1 for Digest {
    fn update(&mut self, data: &[u8]) {
        sha1::Digest::update(&mut self.0, data);
    }

    fn finalize(self) -> [u8; 20] {
        sha1::Digest::finalize(self.0).into()
    }
}

/// What a conversion produced: the file, what the conversion said about it, the progress it
/// reported, and what reading the file back came to.
struct Converted {
    file: Vec<u8>,
    report: ConversionReport,
    progress: Vec<Progress>,
    summary: Summary,
    /// The blocks the file's own header starts its chapters at.
    chapters: Vec<u32>,
}

/// Runs a conversion into memory and hands over everything it produced, refused or not.
fn run(
    inputs: Vec<Input>,
    opts: &Conversion,
) -> (
    Vec<u8>,
    Vec<Progress>,
    Result<ConversionReport, ConvertError>,
) {
    let mut file = Cursor::new(Vec::new());
    let mut progress = Vec::new();
    let report = convert(inputs, opts, AUDIO_ID, &mut file, &mut |event| {
        progress.push(event);
    });

    (file.into_inner(), progress, report)
}

/// A conversion that came out a file, read back and held to everything a TAF states about itself.
fn validated(inputs: Vec<Input>, opts: &Conversion) -> Converted {
    let (file, progress, report) = run(inputs, opts);
    let report = report.expect("the conversion came out a file");
    let (summary, chapters) = validate(&file);

    // What the conversion said about its chapters is what the file itself states about them.
    let pages: Vec<u32> = report
        .chapters
        .iter()
        .map(|chapter| chapter.page.get())
        .collect();
    assert_eq!(pages, chapters, "the report's chapters are the file's");
    assert_eq!(summary.chapters_seen as usize, chapters.len());
    assert_eq!(file.len(), BLOCK_LEN + summary.audio_bytes as usize);

    Converted {
        file,
        report,
        progress,
        summary,
        chapters,
    }
}

/// The file read the way a box reads one: every block of the audio region through `taf`'s
/// validator, hashed as it goes past, with the chapter starts it met.
fn validate(file: &[u8]) -> (Summary, Vec<u32>) {
    let header = HeaderView::parse(&file[..BLOCK_LEN]).expect("a TAF opens with a header block");
    let mut digest = Digest(<sha1::Sha1 as sha1::Digest>::new());
    let mut validator = Validator::new(&header);
    let mut chapters = Vec::new();

    for (at, block) in file[BLOCK_LEN..].chunks(BLOCK_LEN).enumerate() {
        let met = validator
            .push_block(block, Some(&mut digest))
            .unwrap_or_else(|error| panic!("block {at} of the audio region: {error}"));
        if let Some(chapter) = met {
            chapters.push(chapter.block.get());
        }
    }

    let summary = validator
        .finish(Some(digest.finalize()))
        .expect("the file is the one its header describes");

    (summary, chapters)
}

/// The audio the file carries, decoded back to interleaved 48 kHz stereo by libopus.
///
/// Everything the pages behind the two the Opus stream opens with carry, with the pre-skip those
/// state dropped off the front — so frame `n` here is frame `n` of what went into the encoder.
fn decoded(file: &[u8]) -> Vec<i16> {
    let mut decoder = opus::Decoder::new(RATE, opus::Channels::Stereo).expect("a decoder");
    let mut samples = Vec::new();
    let mut at = BLOCK_LEN;

    while at < file.len() {
        let page = PageView::parse(&file[at..]).expect("a page of the audio region");
        if page.sequence() >= 2 {
            for packet in page.packets().take(256) {
                let mut frame = vec![0_i16; FRAME as usize * 2];
                let frames = decoder
                    .decode(packet, &mut frame, false)
                    .expect("the packet decodes");
                frame.truncate(frames * 2);
                samples.extend_from_slice(&frame);
            }
        }
        at += page.total_len();
    }

    samples.drain(..usize::from(OPUS_PRE_SKIP) * 2);

    samples
}

/// The loudest sample of the decoded stream between two frames of it.
fn peak(samples: &[i16], frames: std::ops::Range<usize>) -> i32 {
    samples[frames.start * 2..frames.end * 2]
        .iter()
        .map(|sample| i32::from(*sample).abs())
        .max()
        .unwrap_or(0)
}

/// Where the report says its chapters begin.
fn starts(report: &ConversionReport) -> Vec<Duration> {
    report
        .chapters
        .iter()
        .map(|chapter| chapter.start)
        .collect()
}

/// What the report calls its chapters.
fn titles(report: &ConversionReport) -> Vec<Option<&str>> {
    report
        .chapters
        .iter()
        .map(|chapter| chapter.title.as_deref())
        .collect()
}

/// Holds a duration to one the conversion could not have got further than `slack` from.
fn close(actual: Duration, expected: Duration, slack: Duration) {
    let apart = actual
        .max(expected)
        .checked_sub(actual.min(expected))
        .unwrap_or_default();

    assert!(
        apart <= slack,
        "{actual:?} is {apart:?} away from {expected:?}, which is more than {slack:?}"
    );
}

/// How long `frames` frames at 48 kHz play.
fn plays(frames: u64) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(RATE))
}

#[test]
fn a_wav_becomes_a_taf_of_the_audio_it_carried() {
    let taf = validated(
        vec![input(fixtures::sine_wav(), "sine.wav")],
        &Conversion::default(),
    );

    // Two seconds at 44 100 Hz are 96 000 frames at 48 kHz, which is 34 packets of 60 ms with the
    // last of them holding the 960 frames left over and silence behind them.
    assert_eq!(taf.summary.total_samples, 34 * FRAME);
    assert_eq!(taf.chapters, [0]);
    assert_eq!(starts(&taf.report), [Duration::ZERO]);
    assert_eq!(titles(&taf.report), [None]);
    assert_eq!(taf.report.audio_id, AUDIO_ID);
    assert!(taf.report.cover.is_none());
    close(taf.report.duration, Duration::from_secs(2), FRAME_TIME);
    assert_eq!(taf.summary.audio_bytes % BLOCK_LEN as u32, 0);
}

#[test]
fn an_m4b_becomes_the_chapters_and_the_cover_it_was_authored_with() {
    let taf = validated(
        vec![input(fixtures::TINY_M4B.to_vec(), "tiny.m4b")],
        &Conversion::default(),
    );

    // The marks the container states, each rounded up to the 60 ms frame its chapter's audio
    // begins one of.
    assert_eq!(taf.report.chapters.len(), 2);
    close(starts(&taf.report)[0], Duration::ZERO, FRAME_TIME);
    close(starts(&taf.report)[1], Duration::from_secs(5), FRAME_TIME);
    assert_eq!(
        titles(&taf.report),
        [
            Some(fixtures::M4B_FIRST_TITLE),
            Some(fixtures::M4B_SECOND_TITLE)
        ]
    );

    let cover = taf.report.cover.expect("the m4b carries a cover");
    assert_eq!(cover.mime, "image/png");
    assert_eq!(cover.bytes, fixtures::COVER_PNG);
}

#[test]
fn every_input_of_a_conversion_of_several_begins_a_chapter_where_it_begins() {
    let taf = validated(
        vec![
            input(tone_wav(24_000), "one.wav"),
            input(tone_wav(48_000), "two.wav"),
            input(tone_wav(12_000), "three.wav"),
        ],
        &Conversion::default(),
    );

    // Every chapter begins one 60 ms frame in front of where its input's audio does, which is what
    // padding the frame in hand out to a whole one costs — once per boundary passed.
    assert_eq!(taf.chapters.len(), 3);
    close(starts(&taf.report)[0], Duration::ZERO, Duration::ZERO);
    close(starts(&taf.report)[1], plays(24_000), FRAME_TIME);
    close(starts(&taf.report)[2], plays(72_000), FRAME_TIME * 2);
    assert!(taf.chapters.is_sorted_by(|earlier, later| earlier < later));
    assert_eq!(titles(&taf.report), [None, None, None]);
}

#[test]
fn the_silence_operations_reach_the_audio_the_file_carries() {
    let mut samples = silence(24_000);
    samples.extend(tone(48_000));

    let taf = validated(
        vec![input(wav(&samples), "pause.wav")],
        &Conversion {
            silence: SilenceOpts {
                trim_leading: true,
                add_pause_leading: 48_000,
                ..SilenceOpts::default()
            },
            ..Conversion::default()
        },
    );

    // The half second the recording idles for is gone and the second that was asked for is in
    // front of the tone instead — the chapter it belongs to being the one the file opens with.
    assert_eq!(taf.chapters, [0]);
    assert_eq!(starts(&taf.report), [Duration::ZERO]);

    let audio = decoded(&taf.file);
    assert!(audio.len() / 2 >= 96_000);
    assert!(peak(&audio, 0..47_500) < QUIET, "the pause is silence");
    assert!(peak(&audio, 48_500..95_000) > LOUD, "the tone is behind it");
}

#[test]
fn the_conversion_reports_what_it_is_doing_as_it_runs() {
    let taf = validated(
        vec![
            input(tone_wav(24_000), "one.wav"),
            input(tone_wav(24_000), "two.wav"),
            input(tone_wav(24_000), "three.wav"),
        ],
        &Conversion::default(),
    );

    let decoding: Vec<usize> = taf
        .progress
        .iter()
        .filter_map(|event| match event {
            Progress::Decoding { input_index } => Some(*input_index),
            _ => None,
        })
        .collect();
    let encoded: Vec<u64> = taf
        .progress
        .iter()
        .filter_map(|event| match event {
            Progress::Encoded { samples_done } => Some(*samples_done),
            _ => None,
        })
        .collect();

    // Every input is announced once, in the order they play, and the encoded audio only ever
    // grows — up to what the file came to hold.
    assert_eq!(decoding, [0, 1, 2]);
    assert!(encoded.len() > 1);
    assert!(
        encoded.is_sorted(),
        "the samples done only ever grow: {encoded:?}"
    );
    assert!(encoded.last().is_some_and(|done| *done >= 72_000));
    assert_eq!(taf.progress.last(), Some(&Progress::Finalizing));
    assert!(plays(*encoded.last().unwrap()) <= taf.report.duration + FRAME_TIME);
}

#[test]
fn a_title_belongs_to_the_mark_it_was_authored_on_and_not_to_the_chapter_beside_it() {
    // The fixture's own marks, moved: the atom now states 5 s first and 2 s behind it, and neither
    // of them is at the start of the recording.
    let m4b = fixtures::m4b_with_marks([50_000_000, 20_000_000]);

    let taf = validated(vec![input(m4b, "shuffled.m4b")], &Conversion::default());

    // So the plan is the chapter every TAF opens with, which no mark named, and then the two marks
    // in the order they play — each carrying the title it was authored with rather than the one
    // that happens to sit beside it in the list.
    assert_eq!(
        titles(&taf.report),
        [
            None,
            Some(fixtures::M4B_SECOND_TITLE),
            Some(fixtures::M4B_FIRST_TITLE)
        ]
    );
    close(starts(&taf.report)[1], Duration::from_secs(2), FRAME_TIME);
    close(starts(&taf.report)[2], Duration::from_secs(5), FRAME_TIME);
}

#[test]
fn a_chapter_that_came_out_where_the_one_behind_it_begins_is_the_one_behind_it() {
    // Six seconds off the front of a book whose second chapter begins at five: both marks land
    // where the skip ended, which is one place, and one place is one chapter.
    let taf = validated(
        vec![input(fixtures::TINY_M4B.to_vec(), "tiny.m4b")],
        &Conversion {
            silence: SilenceOpts {
                skip_leading: 6 * u64::from(RATE),
                ..SilenceOpts::default()
            },
            ..Conversion::default()
        },
    );

    assert_eq!(taf.chapters, [0]);
    assert_eq!(starts(&taf.report), [Duration::ZERO]);
    // The chapter that is played there is the one whose audio is played there.
    assert_eq!(titles(&taf.report), [Some(fixtures::M4B_SECOND_TITLE)]);
}

#[test]
fn an_input_that_is_nothing_but_silence_leaves_its_chapter_to_the_input_behind_it() {
    let taf = validated(
        vec![
            input(tone_wav(24_000), "one.wav"),
            input(wav(&silence(24_000)), "two.wav"),
            input(tone_wav(24_000), "three.wav"),
        ],
        &Conversion {
            silence: SilenceOpts {
                trim_each_chapter: true,
                ..SilenceOpts::default()
            },
            ..Conversion::default()
        },
    );

    // The middle input trims to no audio at all, so where its chapter would have begun is where
    // the last input's begins — and the file holds that place once.
    assert_eq!(taf.chapters.len(), 2);
    close(starts(&taf.report)[1], plays(24_000), FRAME_TIME);
}

#[test]
fn an_explicit_plan_puts_the_chapters_where_it_says_and_pads_the_frame_it_lands_in() {
    let taf = validated(
        vec![input(tone_wav(96_000), "tone.wav")],
        &Conversion {
            chapter_mode: ChapterMode::Explicit(vec![48_000]),
            ..Conversion::default()
        },
    );

    // A second in is not a whole number of 60 ms frames, so the frame the boundary falls in is
    // filled out with silence and the chapter's audio begins the frame behind it: 1.02 s.
    assert_eq!(taf.report.chapters.len(), 2);
    close(
        starts(&taf.report)[1],
        Duration::from_millis(1_020),
        FRAME_TIME,
    );
    assert_eq!(titles(&taf.report), [None, None]);

    // Which is audible in the file as the only silence in a stream of tone: the 960 frames that
    // filled the frame out, and the tone going on behind them.
    let audio = decoded(&taf.file);
    assert!(
        peak(&audio, 47_000..47_900) > LOUD,
        "the tone up to the mark"
    );
    assert!(peak(&audio, 48_100..48_900) < FADED, "the frame filled out");
    assert!(peak(&audio, 49_200..50_000) > LOUD, "the chapter's audio");
}

#[test]
fn an_explicit_offset_is_counted_over_the_inputs_together_and_not_over_one_of_them() {
    let taf = validated(
        vec![
            input(tone_wav(24_000), "one.wav"),
            input(tone_wav(24_000), "two.wav"),
        ],
        &Conversion {
            // Three quarters of a second in, which is halfway through the second input: a plan the
            // caller states runs the inputs as the one stream it counts its offsets over.
            chapter_mode: ChapterMode::Explicit(vec![36_000]),
            ..Conversion::default()
        },
    );

    assert_eq!(taf.report.chapters.len(), 2);
    close(starts(&taf.report)[1], plays(36_000), FRAME_TIME);
    // And the boundary between the two inputs is no chapter of its own, since the plan states
    // where the chapters are.
    assert_eq!(taf.chapters.len(), 2);
}

#[test]
fn a_second_mark_where_the_first_one_is_is_that_chapter_rather_than_another() {
    // Both of the fixture's marks moved to two seconds in, which is one place — and a place a
    // chapter begins at is a place one chapter begins at.
    let m4b = fixtures::m4b_with_marks([20_000_000, 20_000_000]);

    let taf = validated(vec![input(m4b, "doubled.m4b")], &Conversion::default());

    assert_eq!(taf.report.chapters.len(), 2);
    assert_eq!(titles(&taf.report), [None, Some(fixtures::M4B_FIRST_TITLE)]);
}

#[test]
fn an_input_whose_shape_nothing_can_bring_to_48_khz_stereo_names_itself_and_says_why() {
    let (_, _, report) = run(
        vec![
            input(tone_wav(4_800), "good.wav"),
            // A tenth of the rate everything is brought to is the slowest a recording is read at,
            // and this is below it.
            input(fixtures::wav_of(4_000, 1, &[0; 400]), "slow.wav"),
        ],
        &Conversion {
            // Which puts the second input's opening in the middle of the stream rather than at the
            // start of one of its own.
            chapter_mode: ChapterMode::Explicit(Vec::new()),
            ..Conversion::default()
        },
    );

    let refusal = report.expect_err("nothing can be made of a stream at that rate");
    assert_eq!(refusal.to_string(), "input 'slow.wav' failed");
    assert_eq!(
        refusal
            .source()
            .expect("what went wrong with it")
            .to_string(),
        "unsupported sample rate 4000"
    );
}

#[test]
fn an_output_that_cannot_be_written_to_is_stated_as_the_file_failing_and_not_the_format() {
    // Room for the block a file reserves for its header and nothing else, so the first page of the
    // Opus stream has nowhere to go.
    let opening = convert(
        vec![input(tone_wav(4_800), "tone.wav")],
        &Conversion::default(),
        AUDIO_ID,
        Full::with(BLOCK_LEN),
        &mut |_| {},
    );
    // And room for a few blocks of it, so the stream begins and the audio runs out of room.
    let midway = convert(
        vec![input(tone_wav(96_000), "tone.wav")],
        &Conversion::default(),
        AUDIO_ID,
        Full::with(BLOCK_LEN * 3),
        &mut |_| {},
    );

    for refusal in [opening, midway] {
        let refusal = refusal.expect_err("the output had no room");
        assert_eq!(refusal.to_string(), "output i/o failed");
        assert!(matches!(refusal, ConvertError::Io(_)), "{refusal:?}");
    }
}

#[test]
fn an_explicit_offset_the_audio_never_reached_is_refused_once_it_is_known_it_never_will_be() {
    let (file, _, report) = run(
        vec![input(tone_wav(48_000), "tone.wav")],
        &Conversion {
            chapter_mode: ChapterMode::Explicit(vec![48_000]),
            ..Conversion::default()
        },
    );

    // Where the audio ends is not a place a chapter begins, and how much audio there is only the
    // end of the stream says — so this is stated with the numbers the conversion found.
    let refusal = report.expect_err("an offset at the end of the audio is no chapter");
    assert_eq!(
        refusal.to_string(),
        "explicit chapter at 48000 beyond total length 48000"
    );
    // The file was written up to that point and never finished, so its header block is the zeros
    // that were reserved for it.
    assert!(file[..BLOCK_LEN].iter().all(|byte| *byte == 0));
}

#[test]
fn explicit_offsets_that_do_not_strictly_increase_are_refused_before_anything_is_written() {
    let (file, progress, report) = run(
        vec![input(tone_wav(48_000), "tone.wav")],
        &Conversion {
            chapter_mode: ChapterMode::Explicit(vec![100, 100]),
            ..Conversion::default()
        },
    );

    let refusal = report.expect_err("two chapters in one place are no plan");
    assert_eq!(
        refusal.to_string(),
        "chapter offsets must be strictly increasing"
    );
    assert!(file.is_empty());
    assert!(progress.is_empty());
}

#[test]
fn a_conversion_with_no_inputs_is_nothing_to_convert() {
    let (file, _, report) = run(Vec::new(), &Conversion::default());

    assert_eq!(
        report.expect_err("there is nothing to convert").to_string(),
        "no inputs"
    );
    assert!(file.is_empty());
}

#[test]
fn a_conversion_that_came_out_with_no_audio_is_still_a_file_of_one_chapter() {
    let taf = validated(
        vec![input(tone_wav(24_000), "tone.wav")],
        &Conversion {
            silence: SilenceOpts {
                // Further in than the recording reaches, so nothing at all is left of it.
                skip_leading: 10 * u64::from(RATE),
                ..SilenceOpts::default()
            },
            ..Conversion::default()
        },
    );

    // A TAF's first block holds the two Opus header pages and an audio page, so a file has at
    // least one packet in it — one 60 ms frame of silence, here.
    assert_eq!(taf.summary.total_samples, FRAME);
    assert_eq!(taf.summary.audio_bytes, BLOCK_LEN as u32);
    assert_eq!(taf.chapters, [0]);
    assert_eq!(starts(&taf.report), [Duration::ZERO]);
    assert!(taf.report.duration < FRAME_TIME);
}

#[test]
fn an_input_that_does_not_decode_names_itself() {
    let (_, _, report) = run(
        vec![
            input(tone_wav(4_800), "good.wav"),
            input(fixtures::broken_adpcm_wav(), "broken.wav"),
        ],
        &Conversion::default(),
    );

    let refusal = report.expect_err("the second input does not decode");
    assert!(
        refusal.to_string().contains("broken.wav"),
        "the refusal names the input it happened in: {refusal}"
    );
}
