//! The application layer end to end: paths in, a `.taf` and the cover beside it out.
//!
//! Every conversion here runs on `taf-encode`'s committed fixtures, by path, and writes into a
//! temporary directory of its own — so what a test states about a file is what is on the disk when
//! it has run, and nothing it wrote outlives it.
//!
//! The file each of them produced is read back the way anything that reads a TAF reads one: `taf`'s
//! own validator over the blocks of the audio region, with the SHA-1 the header states checked
//! against the bytes that were written. So nothing below says anything about a conversion that is
//! not first a file which holds up.

// The casts are on a clock the test reads itself, and every index is into a file the conversion
// just wrote.
#![allow(
    clippy::cast_possible_truncation,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use taf::digest::Sha1;
use taf::header::{HeaderView, BLOCK_LEN};
use taf::reader::{Summary, Validator};
use taf_encode::{ChapterError, Conversion, ConvertError, Progress};
use taffle::{default_output_path, run_convert, ConvertJob, JobError, JobOutcome};
use tempfile::TempDir;

/// The audiobook every conversion here runs on: 10 seconds of AAC in an MP4, two chapters, and a
/// PNG cover.
const BOOK: &str = "tiny.m4b";

/// The cover that book carries, which is what lands beside the file the conversion wrote.
const COVER: &str = "cover.png";

/// The samples of one channel one Opus packet of a TAF carries: 60 ms at 48 kHz.
const FRAME: u64 = 2_880;

/// The fixture `name`, where `taf-encode` keeps the committed ones.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../taf-encode/tests/fixtures")
        .join(name)
}

/// A job over `inputs` with everything else left as a frontend leaves it: the engine's defaults,
/// and the cover written beside the file.
fn job(inputs: Vec<PathBuf>, output: Option<PathBuf>) -> ConvertJob {
    ConvertJob {
        inputs,
        output,
        options: Conversion::default(),
        write_cover: true,
    }
}

/// Runs `job` and hands over what it came to, with every progress event it reported.
fn run(job: ConvertJob) -> (Result<JobOutcome, JobError>, Vec<Progress>) {
    let mut progress = Vec::new();
    let outcome = run_convert(job, &mut |event| progress.push(event));

    (outcome, progress)
}

/// What time it is, the way the audio id of a conversion counts it.
fn unix_now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock set behind 1970")
        .as_secs() as u32
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

/// The file at `path` read the way a box reads one: every block of the audio region through `taf`'s
/// validator, hashed as it goes past, with the chapter starts it met and the audio id its header
/// states.
fn validate(path: &Path) -> (Summary, Vec<u32>, u32) {
    let file = fs::read(path).expect("the conversion wrote a file");
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

    (summary, chapters, header.audio_id().get())
}

#[test]
fn a_conversion_is_named_after_its_first_input_with_the_format_in_the_place_of_its_extension() {
    assert_eq!(
        default_output_path(Path::new("/x/Book Name.m4b")),
        Path::new("/x/Book Name.taf")
    );
}

#[test]
fn a_name_with_no_extension_keeps_all_of_itself_and_the_format_is_added_to_it() {
    // Which is what `.taf` in the place of the extension means where there is none to replace, and
    // the only thing that keeps two files called `Book` and `Book.m4b` apart on the way out.
    assert_eq!(
        default_output_path(Path::new("/x/Book Name")),
        Path::new("/x/Book Name.taf")
    );
    // A name of several dots keeps every one of them but the last: only the extension goes.
    assert_eq!(
        default_output_path(Path::new("/x/Book Name.Teil 2.m4b")),
        Path::new("/x/Book Name.Teil 2.taf")
    );
    // And a file named without a directory stays where it was named, which is beside itself.
    assert_eq!(
        default_output_path(Path::new("Book Name.m4b")),
        Path::new("Book Name.taf")
    );
}

#[test]
fn a_book_converts_into_a_taf_beside_it_carrying_the_cover_it_came_with() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = dir.path().join(BOOK);
    fs::copy(fixture(BOOK), &book).expect("the fixture copies in");

    let before = unix_now();
    let (outcome, progress) = run(job(vec![book], None));
    let after = unix_now();

    let outcome = outcome.expect("the book converts");
    assert_eq!(outcome.taf_path, dir.path().join("tiny.taf"));

    // The file itself: every block of it, hashed and held to what its header says about it.
    let (summary, chapters, audio_id) = validate(&outcome.taf_path);
    assert_eq!(
        summary.chapters_seen as usize,
        outcome.report.chapters.len()
    );
    let pages: Vec<u32> = outcome
        .report
        .chapters
        .iter()
        .map(|chapter| chapter.page.get())
        .collect();
    assert_eq!(pages, chapters, "the report's chapters are the file's");
    // The book states two of them, at its start and five seconds in.
    assert_eq!(chapters.len(), 2);
    assert_eq!(outcome.report.duration.as_secs(), 10);

    // The audio id is the clock, read once, here: what the file states is what the report states,
    // and it lies between the two readings the test took around the conversion.
    assert_eq!(audio_id, outcome.report.audio_id.get());
    assert!(
        (before..=after).contains(&audio_id),
        "the audio id {audio_id} is no time between {before} and {after}"
    );

    // The cover, beside the file under the file's own name, byte for byte what the book carried.
    assert_eq!(outcome.cover_path, Some(dir.path().join("tiny.png")));
    assert_eq!(outcome.cover_error, None);
    let written = fs::read(dir.path().join("tiny.png")).expect("the cover is beside the file");
    assert_eq!(
        written,
        fs::read(fixture(COVER)).expect("the fixture cover")
    );

    // What the engine reported, as it reported it: the first input reached, the audio growing, and
    // the file being closed at the end of it.
    assert_eq!(
        progress.first(),
        Some(&Progress::Decoding { input_index: 0 })
    );
    assert_eq!(progress.last(), Some(&Progress::Finalizing));
    let encoded: Vec<u64> = progress
        .iter()
        .filter_map(|event| match event {
            Progress::Encoded { samples_done } => Some(*samples_done),
            _ => None,
        })
        .collect();
    assert!(!encoded.is_empty(), "the conversion encoded nothing");
    // The count only ever grows, and stands still where a block of audio did not fill out a frame.
    assert!(
        encoded.windows(2).all(|pair| pair[0] <= pair[1]),
        "the audio encoded so far went backwards"
    );
    // The last of them is the file, less the frame the conversion was still holding when the audio
    // ran out and closing the file filled in.
    let last = *encoded.last().expect("something was encoded");
    assert!(
        (last..=last + FRAME).contains(&summary.total_samples),
        "the file carries {} where {last} was reported",
        summary.total_samples
    );
}

#[test]
#[cfg(unix)]
fn a_cover_that_cannot_be_written_is_reported_and_the_conversion_still_stands() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("a directory of its own");
    let taf_path = dir.path().join("Book.taf");
    // The output is made in front of the directory being closed: writing into a file that is
    // already there needs nothing of the directory around it, and the cover is a new file.
    fs::File::create(&taf_path).expect("the output is made");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555))
        .expect("the directory closes");

    // A closed directory is only closed to a user its mode applies to: root carries
    // CAP_DAC_OVERRIDE and writes into it regardless, so under one there is no refused write for
    // this test to be about at all. The probe finds that out instead of the test asserting
    // something that was never going to happen.
    let probe = dir.path().join("probe");
    if fs::write(&probe, b"").is_ok() {
        fs::remove_file(&probe).expect("the probe goes away again");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("the directory opens again");

        return;
    }

    let (outcome, _) = run(job(vec![fixture(BOOK)], Some(taf_path.clone())));

    // Whatever the run came to, the directory opens again — a temporary one that cannot be written
    // to cannot be cleaned up either.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
        .expect("the directory opens again");

    let outcome = outcome.expect("the conversion stands whatever became of the cover");
    assert_eq!(outcome.cover_path, None);
    let why = outcome
        .cover_error
        .expect("the cover says why it is not there");
    assert!(
        why.contains(&dir.path().join("Book.png").display().to_string()),
        "the cover failure names no file: {why}"
    );
    assert!(!dir.path().join("Book.png").exists());

    // And the book itself came out whole, which is the whole point of not failing the run.
    let (_, chapters, _) = validate(&taf_path);
    assert_eq!(chapters.len(), 2);
}

#[test]
fn a_conversion_named_like_its_own_cover_keeps_the_book_and_leaves_the_picture_out() {
    let dir = TempDir::new().expect("a directory of its own");
    // The output the caller named, which is where the cover of a book carrying a PNG would go.
    let taf_path = dir.path().join("Book.png");

    let (outcome, _) = run(job(vec![fixture(BOOK)], Some(taf_path.clone())));

    let outcome = outcome.expect("the book converts whatever its file is called");
    assert_eq!(outcome.taf_path, taf_path);
    assert_eq!(outcome.cover_path, None);
    let why = outcome
        .cover_error
        .expect("the cover says why it is not there");
    assert!(
        why.contains(&taf_path.display().to_string()),
        "the cover failure names no file: {why}"
    );

    // The book is what is at that name: the picture was never written over it.
    let (_, chapters, _) = validate(&taf_path);
    assert_eq!(chapters.len(), 2);
    assert_eq!(
        fs::read_dir(dir.path())
            .expect("the directory reads")
            .count(),
        1,
        "something other than the book is in the directory"
    );
}

#[test]
fn a_conversion_told_to_leave_the_cover_alone_writes_no_cover_at_all() {
    let dir = TempDir::new().expect("a directory of its own");
    let taf_path = dir.path().join("Book.taf");

    let (outcome, _) = run(ConvertJob {
        write_cover: false,
        ..job(vec![fixture(BOOK)], Some(taf_path.clone()))
    });

    let outcome = outcome.expect("the book converts");
    // Nothing was written and nothing went wrong: a cover that was never asked for is not a
    // failure to report.
    assert_eq!(outcome.cover_path, None);
    assert_eq!(outcome.cover_error, None);
    assert!(!dir.path().join("Book.png").exists());
    assert_eq!(
        fs::read_dir(dir.path())
            .expect("the directory reads")
            .count(),
        1,
        "something other than the TAF is in the directory"
    );
    validate(&taf_path);
}

#[test]
fn an_input_that_is_not_there_is_refused_before_anything_is_written() {
    let dir = TempDir::new().expect("a directory of its own");
    let missing = dir.path().join("nowhere.m4b");
    let taf_path = dir.path().join("Book.taf");

    let (outcome, progress) = run(job(vec![missing.clone()], Some(taf_path.clone())));

    let error = outcome.expect_err("a file that is not there is no input");
    match &error {
        JobError::OpenInput { path, source } => {
            assert_eq!(*path, missing);
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        error.to_string(),
        format!("cannot open input {}", missing.display())
    );
    // The inputs are opened in front of the output, so a job that never ran left nothing behind.
    assert!(
        !taf_path.exists(),
        "an output was made for a job that failed"
    );
    assert!(progress.is_empty());
}

#[test]
fn an_output_that_cannot_be_made_is_refused_under_the_name_it_was_given() {
    let dir = TempDir::new().expect("a directory of its own");
    // A directory nothing made: what a conversion writes into is created, and the place it is
    // created in is not.
    let taf_path = dir.path().join("nowhere").join("Book.taf");

    let (outcome, progress) = run(job(vec![fixture(BOOK)], Some(taf_path.clone())));

    let error = outcome.expect_err("there is nowhere to write the file");
    match &error {
        JobError::CreateOutput { path, source } => {
            assert_eq!(*path, taf_path);
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        error.to_string(),
        format!("cannot create output {}", taf_path.display())
    );
    assert!(
        progress.is_empty(),
        "a conversion that never began reported"
    );
}

#[test]
fn an_input_that_fails_is_named_by_the_path_the_caller_stated() {
    let dir = TempDir::new().expect("a directory of its own");
    let book = fixture("no-audio.mp4");

    let (outcome, _) = run(job(vec![book.clone()], Some(dir.path().join("Book.taf"))));

    let error = outcome.expect_err("an MP4 of one video track is no audiobook");
    match &error {
        JobError::Convert(ConvertError::Input { name, .. }) => {
            assert_eq!(*name, book.display().to_string());
        }
        other => panic!("{other:?}"),
    }
    // Which is what a user is shown: the path they typed, and not a bare file name that two
    // directories could both hold.
    assert_eq!(
        error.to_string(),
        format!("input '{}' failed", book.display())
    );
}

#[test]
fn a_job_of_no_inputs_is_the_engine_s_own_refusal_and_no_file_is_made_for_it() {
    let dir = TempDir::new().expect("a directory of its own");
    let taf_path = dir.path().join("Book.taf");

    let (outcome, progress) = run(job(Vec::new(), Some(taf_path.clone())));

    let error = outcome.expect_err("there is nothing to convert");
    assert!(
        matches!(
            error,
            JobError::Convert(ConvertError::Chapters(ChapterError::Empty))
        ),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "no inputs");
    assert!(!taf_path.exists(), "an output was made for an empty job");
    assert!(progress.is_empty());
}
