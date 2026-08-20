//! The binary as a user meets it: `taffle` run over files in a temporary directory of its own, and
//! what it left behind read back the way anything that reads a TAF reads one.
//!
//! Every conversion here runs on audio this file builds — WAV bytes written out by hand, so a test
//! that is about a duration or a chapter offset states the audio it is about right here — except
//! where the cover art matters, which no hand-written WAV carries: those run on `taf-encode`'s
//! committed m4b.
//!
//! What a run produced is checked by `taf`'s own validator over every block of the audio region.
//! The SHA-1 the header states is left alone here: what a file hashes to is the writer's business
//! and pinned where the writer is, and nothing a frontend does can make a structurally sound file
//! hash differently.

// The tone is bounded by the peak it is scaled with, every index is into a file the run just wrote,
// and a test that cannot set itself up has nothing to say.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::f64::consts::TAU;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::{contains, starts_with};
use taf::header::{HeaderView, BLOCK_LEN};
use taf::reader::Validator;
use tempfile::TempDir;

/// The rate the WAV files below are written at — not the 48 kHz a TAF carries, so every conversion
/// here goes through the resampler the way a real recording does.
const RATE: u32 = 16_000;

/// What the tone peaks at: loud enough that nothing in it is taken for silence.
const PEAK: f64 = 12_000.0;

/// The chapters a Toniebox plays, which is what a longer list is warned about.
const MAX_CHAPTERS: usize = 99;

/// The audiobook the cover tests run on: 10 seconds of AAC in an MP4, two chapters, and a PNG
/// cover.
const BOOK: &str = "tiny.m4b";

/// The TAF the `info` tests read: ten seconds of sine, written by teddycloud itself.
const GOLDEN: &str = "golden-sine.taf";

/// The binary this crate builds, ready to be run.
fn taffle() -> Command {
    Command::cargo_bin("taffle").expect("the binary builds")
}

/// The fixture `name`, where `taf-encode` keeps the committed ones.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../taf-encode/tests/fixtures")
        .join(name)
}

/// The fixture `name`, where `taf` keeps the committed TAF files.
fn taf_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../taf/tests/fixtures")
        .join(name)
}

/// A 440 Hz sine, `seconds` seconds of it at [`RATE`].
fn tone(seconds: f64) -> Vec<i16> {
    (0..frames(seconds))
        .map(|frame| ((TAU * 440.0 * f64::from(frame) / f64::from(RATE)).sin() * PEAK) as i16)
        .collect()
}

/// Digital silence, `seconds` seconds of it at [`RATE`].
fn silence(seconds: f64) -> Vec<i16> {
    vec![0; frames(seconds) as usize]
}

/// The frames `seconds` seconds come to at [`RATE`].
fn frames(seconds: f64) -> u32 {
    (f64::from(RATE) * seconds) as u32
}

/// Writes `samples` into `dir` as a mono WAV called `name`, and hands over where it went.
///
/// A WAV file is 44 bytes of RIFF header — the format chunk and the data chunk header — and the
/// samples behind it, little-endian.
fn wav(dir: &Path, name: &str, samples: &[i16]) -> PathBuf {
    let data_len = samples.len() as u32 * 2;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);

    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");

    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // format tag 1: uncompressed PCM
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // one channel
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 2).to_le_bytes()); // bytes per second
    bytes.extend_from_slice(&2_u16.to_le_bytes()); // bytes per frame
    bytes.extend_from_slice(&16_u16.to_le_bytes()); // bits per sample

    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    let path = dir.join(name);
    fs::write(&path, bytes).expect("the WAV is written");

    path
}

/// The chapters the TAF at `path` holds, with the file checked over on the way: every block of its
/// audio region through `taf`'s validator, and the chapter list the header states read back.
fn chapters_of(path: &Path) -> usize {
    let file = fs::read(path).expect("the run wrote a file");
    let header = HeaderView::parse(&file[..BLOCK_LEN]).expect("a TAF opens with a header block");
    let mut validator = Validator::new(&header);

    for (at, block) in file[BLOCK_LEN..].chunks(BLOCK_LEN).enumerate() {
        validator
            .push_block(block, None)
            .unwrap_or_else(|error| panic!("block {at} of the audio region: {error}"));
    }
    validator
        .finish(None)
        .expect("the file is the one its header describes");

    header.chapter_count()
}

/// A chapter list of `count` offsets a tenth of a second apart, beginning `from` tenths into the
/// audio.
fn offsets(from: usize, count: usize) -> String {
    (from..from + count)
        .map(|at| format!("{}.{}", at / 10, at % 10))
        .collect::<Vec<String>>()
        .join(",")
}

/// What a run says about `chapters` being more of them than a box plays.
fn over_limit(chapters: usize) -> String {
    format!("warning: {chapters} chapters is more than the {MAX_CHAPTERS} a Toniebox plays")
}

/// What is in `dir`, by file name, in a settled order.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("the directory reads")
        .map(|entry| entry.expect("the entry reads").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();

    names
}

#[test]
fn a_book_converts_into_a_taf_beside_it_with_the_cover_it_came_with() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = dir.path().join(BOOK);
    fs::copy(fixture(BOOK), &book).expect("the fixture copies in");
    let taf = dir.path().join("tiny.taf");

    taffle()
        .arg(&book)
        .assert()
        .success()
        .stdout(contains(taf.display().to_string()));

    // The book states two chapters, and the file that came out holds them.
    assert_eq!(chapters_of(&taf), 2);
    assert_eq!(listing(dir.path()), ["tiny.m4b", "tiny.png", "tiny.taf"]);
}

#[test]
fn a_run_told_to_leave_the_cover_alone_writes_the_book_and_nothing_else() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = dir.path().join(BOOK);
    fs::copy(fixture(BOOK), &book).expect("the fixture copies in");

    taffle().arg(&book).arg("--no-cover").assert().success();

    assert_eq!(listing(dir.path()), ["tiny.m4b", "tiny.taf"]);
}

#[test]
fn a_cover_that_could_not_be_written_is_said_and_the_book_still_stands() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = dir.path().join(BOOK);
    fs::copy(fixture(BOOK), &book).expect("the fixture copies in");
    // The output named where this book's own cover would go: the picture's place is the file that
    // was just written, so it is left out and said so — and the book that was written stands.
    let taf = dir.path().join("Book.png");

    taffle()
        .arg(&book)
        .arg("-o")
        .arg(&taf)
        .assert()
        .success()
        // The line the run wrote over while it worked is ended in front of the warning, so what is
        // said after a conversion begins on a line of its own.
        .stderr(contains("\nwarning: no cover"));

    assert_eq!(chapters_of(&taf), 2);
    assert_eq!(listing(dir.path()), ["Book.png", "tiny.m4b"]);
}

#[test]
fn the_file_a_run_writes_is_the_one_it_was_told_to_write() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(2.0));
    let taf = dir.path().join("custom.taf");

    taffle()
        .arg(&book)
        .arg("-o")
        .arg(&taf)
        .assert()
        .success()
        .stdout(contains(format!(
            "wrote {} (0:02, 1 chapter)",
            taf.display()
        )))
        // The line the run wrote over while it worked, at what it last said: the seconds of audio
        // that had gone into the file by then, which is the whole frames of it — the last part of
        // a frame goes in as the file is closed, and this is the second before that.
        .stderr(contains("1s encoded"));

    assert_eq!(chapters_of(&taf), 1);
    assert_eq!(listing(dir.path()), ["book.wav", "custom.taf"]);
}

#[test]
fn a_run_that_names_no_file_at_all_is_a_usage_error_pointing_at_the_help() {
    taffle().assert().code(2).stderr(contains("--help"));
}

#[test]
fn an_input_that_is_not_there_fails_the_run_under_the_name_it_was_given() {
    let dir = TempDir::new().expect("a directory of its own");
    let missing = dir.path().join("nowhere.m4b");

    taffle()
        .arg(&missing)
        .assert()
        .code(1)
        .stderr(contains(missing.display().to_string()));

    // Nothing was written for a run that never converted anything.
    assert_eq!(listing(dir.path()), [] as [String; 0]);
}

#[test]
fn the_chapters_a_run_was_given_are_the_chapters_the_file_holds() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(2.0));
    let taf = dir.path().join("book.taf");

    taffle()
        .arg(&book)
        .args(["--chapters", "0:00,0:01"])
        .assert()
        .success()
        .stdout(contains("2 chapters"));

    assert_eq!(chapters_of(&taf), 2);
}

#[test]
fn a_chapter_list_longer_than_a_box_plays_is_a_warning_and_the_file_is_still_written() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(11.0));
    let taf = dir.path().join("book.taf");

    // One chapter more than the box takes. A list somebody typed is settled in front of the audio,
    // so the warning is the first thing said rather than something waited an encoding for.
    taffle()
        .arg(&book)
        .args(["--chapters", &offsets(0, MAX_CHAPTERS + 1)])
        .assert()
        .success()
        .stderr(starts_with(over_limit(MAX_CHAPTERS + 1)));

    assert_eq!(chapters_of(&taf), MAX_CHAPTERS + 1);
}

#[test]
fn the_chapter_a_file_opens_with_counts_towards_what_a_box_plays() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(11.0));
    let taf = dir.path().join("book.taf");

    // As many offsets as the box takes, none of them at the start of the audio — where a TAF
    // begins the chapter every one of them has. So the file holds one more than was typed, and
    // that is the count the warning is about.
    taffle()
        .arg(&book)
        .args(["--chapters", &offsets(1, MAX_CHAPTERS)])
        .assert()
        .success()
        .stderr(starts_with(over_limit(MAX_CHAPTERS + 1)));

    assert_eq!(chapters_of(&taf), MAX_CHAPTERS + 1);
}

#[test]
fn a_chapter_list_of_what_a_box_plays_is_no_warning_at_all() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(11.0));
    let taf = dir.path().join("book.taf");

    // Exactly what the box plays, the first of them at the start of the audio: the limit is where
    // the warning begins and not one chapter short of it.
    taffle()
        .arg(&book)
        .args(["--chapters", &offsets(0, MAX_CHAPTERS)])
        .assert()
        .success()
        .stderr(contains("warning").not());

    assert_eq!(chapters_of(&taf), MAX_CHAPTERS);
}

#[test]
fn more_files_than_a_box_plays_chapters_is_warned_about_once_the_file_is_written() {
    let dir = TempDir::new().expect("a directory of its own");
    // One chapter per file is what a set of them converts to, so this is a chapter list nobody
    // typed — and the count is only known once the conversion has run.
    let books: Vec<PathBuf> = (0..=MAX_CHAPTERS)
        .map(|at| wav(dir.path(), &format!("{at:03}.wav"), &tone(0.2)))
        .collect();

    taffle()
        .args(&books)
        .assert()
        .success()
        .stderr(contains(over_limit(MAX_CHAPTERS + 1)));

    assert_eq!(chapters_of(&dir.path().join("000.taf")), MAX_CHAPTERS + 1);
}

#[test]
fn a_run_whose_output_is_one_of_its_inputs_is_refused_before_a_file_is_opened() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(2.0));
    let before = fs::read(&book).expect("the input reads");

    taffle()
        .arg(&book)
        .arg("-o")
        .arg(&book)
        .assert()
        .code(1)
        .stderr(contains(book.display().to_string()));

    // The input is what it was: a run that was refused wrote over nothing.
    assert_eq!(fs::read(&book).expect("the input reads"), before);
    assert_eq!(listing(dir.path()), ["book.wav"]);
}

#[test]
fn a_taf_converted_where_it_lies_would_be_its_own_output_and_is_refused() {
    let dir = TempDir::new().expect("a directory of its own");
    // What a run over `*.taf` comes to: the name the output is derived from is the input's own.
    let book = wav(dir.path(), "book.taf", &tone(2.0));
    let before = fs::read(&book).expect("the input reads");

    taffle()
        .arg(&book)
        .assert()
        .code(1)
        .stderr(contains(book.display().to_string()));

    assert_eq!(fs::read(&book).expect("the input reads"), before);
}

#[test]
fn the_pauses_a_run_puts_in_front_of_chapter_one_are_added_to_each_other() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(2.0));

    // Two seconds of audio, a second in front of the first chapter and a second in front of every
    // chapter: the first chapter gets both, one behind the other, and the file plays four seconds.
    taffle()
        .arg(&book)
        .args(["--add-pause-leading", "1", "--add-pause-each-chapter", "1"])
        .assert()
        .success()
        .stdout(contains("0:04"));
}

#[test]
fn what_a_run_skips_and_trims_off_the_front_is_gone_from_the_file() {
    let dir = TempDir::new().expect("a directory of its own");
    let mut samples = silence(1.0);
    samples.extend(tone(1.0));
    let book = wav(dir.path(), "book.wav", &samples);

    // A second of silence in front of a second of tone: skipping the second takes the silence, and
    // trimming it takes the same silence by finding it.
    taffle()
        .arg(&book)
        .args(["--skip-leading", "1.0"])
        .assert()
        .success()
        .stdout(contains("0:01"));
    taffle()
        .arg(&book)
        .arg("--trim-pause-leading")
        .assert()
        .success()
        .stdout(contains("0:01"));
    taffle()
        .arg(&book)
        .arg("--trim-pause-each-chapter")
        .assert()
        .success()
        .stdout(contains("0:01"));
}

#[test]
fn the_help_states_what_stacks_and_what_a_cover_is_written_over() {
    // The two things a user cannot see from the flag names alone: the pauses add to one another at
    // the first chapter, and the cover goes to a name of its own that it takes whatever is there.
    taffle()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Stacks").and(contains("overwriting")));
}

#[test]
fn a_duration_that_is_no_duration_is_a_usage_error() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(0.5));

    taffle()
        .arg(&book)
        .args(["--skip-leading", "1:99"])
        .assert()
        .code(2)
        .stderr(contains("1:99"));
}

#[test]
fn a_taf_is_read_back_with_everything_its_header_states() {
    let taf = taf_fixture(GOLDEN);

    // 9.66 seconds of sine in 27 blocks of audio, and the one chapter every TAF starts at block 0.
    taffle()
        .arg("info")
        .arg(&taf)
        .assert()
        .success()
        .stdout(format!(
            "{}\n  \
             audio id: 444913029\n  \
             duration: 0:09\n  \
             audio: 110592 bytes\n  \
             chapters: 1\n\
             \x20     #  block  start\n\
             \x20     1      0   0:00\n  \
             valid\n",
            taf.display()
        ));
}

#[test]
fn a_taf_whose_audio_does_not_hash_to_what_its_header_states_is_refused() {
    let dir = TempDir::new().expect("a directory of its own");
    let taf = dir.path().join("tampered.taf");
    let mut bytes = fs::read(taf_fixture(GOLDEN)).expect("the fixture reads");

    // The hash the header states, one bit of it turned over: every page of the file still adds up,
    // so the file's own SHA-1 is the one thing that catches it — which is what audio somebody
    // changed and re-summed the pages of comes to.
    let header = HeaderView::parse(&bytes[..BLOCK_LEN]).expect("a TAF opens with a header block");
    let stated = *header.sha1();
    let at = bytes[..BLOCK_LEN]
        .windows(stated.len())
        .position(|window| window == stated)
        .expect("the header states its hash");
    bytes[at] ^= 0x01;
    fs::write(&taf, &bytes).expect("the copy is written");

    taffle()
        .arg("info")
        .arg(&taf)
        .assert()
        .code(1)
        // The frontend is the one that hashed the file, so it says both hashes: the one the header
        // asks for and the one the audio came to.
        .stderr(
            contains(taf.display().to_string())
                .and(contains("sha1"))
                .and(contains("1 file is not the TAF its header describes")),
        )
        // Nothing is said about a file that is not the one its header describes.
        .stdout("");
}

#[test]
fn a_taf_whose_pages_do_not_add_up_is_refused_at_the_block_they_lie_in() {
    let dir = TempDir::new().expect("a directory of its own");
    let taf = dir.path().join("flipped.taf");
    let mut bytes = fs::read(taf_fixture(GOLDEN)).expect("the fixture reads");

    // A byte of the audio region turned over where no checksum was fixed up after it: the page it
    // lies in no longer sums to what it states, which is caught where the block is read rather
    // than at the file's hash.
    bytes[3 * BLOCK_LEN + 100] ^= 0x01;
    fs::write(&taf, &bytes).expect("the copy is written");

    taffle()
        .arg("info")
        .arg(&taf)
        .assert()
        .code(1)
        .stderr(
            contains(taf.display().to_string())
                .and(contains("block 2 of the audio region"))
                .and(contains("checksum")),
        )
        .stdout("");
}

#[test]
fn a_taf_that_stops_short_is_refused_wherever_it_stops() {
    let dir = TempDir::new().expect("a directory of its own");
    let whole = fs::read(taf_fixture(GOLDEN)).expect("the fixture reads");

    // Cut in the middle of a block, and cut exactly where one ends: the first leaves a read that is
    // no block at all, the second a file that is simply short of the length its header states.
    let cases = [
        (
            "mid-block.taf",
            whole.len() - 2048,
            "2048 bytes are not one",
        ),
        (
            "whole-blocks.taf",
            whole.len() - BLOCK_LEN,
            "106496 bytes, and its header states 110592",
        ),
    ];

    for (name, len, said) in cases {
        let taf = dir.path().join(name);
        fs::write(&taf, &whole[..len]).expect("the copy is written");

        taffle()
            .arg("info")
            .arg(&taf)
            .assert()
            .code(1)
            .stderr(contains(taf.display().to_string()).and(contains(said)))
            .stdout("");
    }
}

#[test]
fn a_file_that_is_no_taf_at_all_is_refused_under_its_own_name() {
    let dir = TempDir::new().expect("a directory of its own");
    // Audio that is not a TAF, a file too short to hold even a header block, a file that is not
    // there at all, and a directory — which opens like a file and then does not read like one.
    let wave = wav(dir.path(), "book.wav", &tone(0.5));
    let stub = dir.path().join("stub.taf");
    fs::write(&stub, b"TAF").expect("the stub is written");
    let missing = dir.path().join("nowhere.taf");
    let folder = dir.path().to_path_buf();

    for path in [&wave, &stub, &missing, &folder] {
        taffle()
            .arg("info")
            .arg(path)
            .assert()
            .code(1)
            .stderr(contains(path.display().to_string()))
            .stdout("");
    }
}

#[test]
fn a_book_this_binary_wrote_reads_back_as_the_book_it_wrote() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = wav(dir.path(), "book.wav", &tone(2.0));
    let taf = dir.path().join("book.taf");

    taffle()
        .arg(&book)
        .args(["--chapters", "0:00,0:01"])
        .assert()
        .success();

    // What the writer wrote, read back through the validator: the file holds the two chapters that
    // were asked for, the second of them a second in, and every block of it adds up.
    taffle().arg("info").arg(&taf).assert().success().stdout(
        contains("duration: 0:02")
            .and(contains("chapters: 2"))
            .and(contains("      1      0   0:00"))
            // The second chapter starts at a block of its own, wherever the packet it begins
            // at put that block.
            .and(contains("\n      2  "))
            .and(contains("valid")),
    );
}

#[test]
fn every_file_named_is_read_and_one_that_is_no_taf_does_not_stop_the_rest() {
    let dir = TempDir::new().expect("a directory of its own");
    let broken = dir.path().join("broken.taf");
    fs::write(&broken, vec![0; 8192]).expect("the file is written");

    // The bad file is said as it is met and the good one behind it is read all the same; the code
    // is the run's, and it is 1 because one of them was not the file its header describes.
    taffle()
        .arg("info")
        .arg(&broken)
        .arg(taf_fixture(GOLDEN))
        .assert()
        .code(1)
        .stdout(contains("audio id: 444913029").and(contains("valid")))
        .stderr(
            contains(broken.display().to_string())
                .and(contains("1 file is not the TAF its header describes")),
        );
}

#[test]
fn the_files_that_were_not_tafs_are_counted_in_what_the_run_comes_to() {
    let dir = TempDir::new().expect("a directory of its own");
    let broken: Vec<PathBuf> = (0..2)
        .map(|at| {
            let path = dir.path().join(format!("broken{at}.taf"));
            fs::write(&path, vec![0; 8192]).expect("the file is written");

            path
        })
        .collect();

    taffle()
        .arg("info")
        .args(&broken)
        .assert()
        .code(1)
        .stderr(contains("2 files are not the TAFs their headers describe"));
}
