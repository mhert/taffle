//! Reading a TAF back: what a file the library is handed turns out to hold, and what it says
//! about one that is not the file its header describes.
//!
//! Every case runs on `taf`'s golden fixture — a file teddycloud itself wrote, hash and all — so
//! what is asserted is what a real TAF holds rather than what this workspace's own writer happens
//! to produce, and the SHA-1 that comes out of the reading is held against the one that came with
//! the file.

// Every index is into a file this test just read, and a test that cannot set itself up has nothing
// to say.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use taf::header::{HeaderView, BLOCK_LEN};
use taffle::{inspect, InspectError};
use tempfile::TempDir;

/// The TAF every case here reads: ten seconds of sine in 27 blocks of audio, one chapter.
const GOLDEN: &str = "golden-sine.taf";

/// The fixture `name`, where `taf` keeps the committed TAF files.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../taf/tests/fixtures")
        .join(name)
}

/// The golden fixture with `tamper` run over its bytes, written into `dir` as `name`.
fn tampered(dir: &Path, name: &str, tamper: impl FnOnce(&mut Vec<u8>)) -> PathBuf {
    let mut bytes = fs::read(fixture(GOLDEN)).expect("the fixture reads");
    tamper(&mut bytes);

    let path = dir.join(name);
    fs::write(&path, &bytes).expect("the copy is written");

    path
}

#[test]
fn a_taf_reads_back_as_everything_its_header_states() {
    let taf = inspect(&fixture(GOLDEN)).expect("the fixture is the file its header describes");

    assert_eq!(taf.audio_id.get(), 444_913_029);
    assert_eq!(taf.audio_bytes, 110_592);
    // 161 Opus frames of 60 ms is 9.66 seconds of audio in the file, and what it *plays* is that
    // less the 312-sample pre-skip a player drops in front of it: 9.6535 s, which is where the
    // fractions of a second matter — a reading that dropped them would say the same 0:09 as one
    // that kept them.
    assert_eq!(taf.duration, Duration::new(9, 653_500_000));
    // The one chapter every TAF has, at the block its audio begins in.
    assert_eq!(taf.chapters.len(), 1);
    assert_eq!(taf.chapters[0].block.get(), 0);
    assert_eq!(taf.chapters[0].start, Duration::ZERO);
}

#[test]
fn a_block_that_does_not_add_up_is_refused_at_the_block_it_lies_in() {
    let dir = TempDir::new().expect("a directory of its own");
    // A byte of the file's fourth block turned over, where no checksum was fixed up after it. The
    // header block is the first of them, so that is block 2 of the audio region — and which block
    // it was is the whole of what this refusal has to carry.
    let taf = tampered(dir.path(), "flipped.taf", |bytes| {
        bytes[3 * BLOCK_LEN + 100] ^= 0x01;
    });

    let why = inspect(&taf).expect_err("a page that does not sum to its checksum is no page");

    assert!(matches!(why, InspectError::Block { at: 2, .. }), "{why:?}");
}

#[test]
fn audio_that_does_not_hash_to_what_the_header_states_carries_both_hashes() {
    let dir = TempDir::new().expect("a directory of its own");
    let whole = fs::read(fixture(GOLDEN)).expect("the fixture reads");
    let header = HeaderView::parse(&whole[..BLOCK_LEN]).expect("a TAF opens with a header block");
    let stated = *header.sha1();

    // The hash the header states, one bit of it turned over: every page of the file still adds up,
    // so the file's own SHA-1 is the one thing that catches it — which is what audio somebody
    // changed and re-summed the pages of comes to.
    let at = whole[..BLOCK_LEN]
        .windows(stated.len())
        .position(|window| window == stated)
        .expect("the header states its hash");
    let taf = tampered(dir.path(), "tampered.taf", |bytes| bytes[at] ^= 0x01);
    let mut asked_for = stated;
    asked_for[0] ^= 0x01;

    let why = inspect(&taf).expect_err("audio that hashes to something else is not that audio");

    // The audio was left alone, so it still hashes to what the file used to state, and the header
    // now asks for that hash with one bit turned over. Which way round they are is what tells a
    // reader where the damage is.
    assert!(
        matches!(
            why,
            InspectError::Sha1Mismatch { stated, hashed } if stated == asked_for && hashed == *header.sha1()
        ),
        "{why:?}"
    );
}

#[test]
fn a_file_that_is_no_taf_at_all_is_refused_before_anything_is_hashed() {
    let dir = TempDir::new().expect("a directory of its own");
    let stub = dir.path().join("stub.taf");
    fs::write(&stub, b"TAF").expect("the stub is written");

    let why = inspect(&stub).expect_err("three bytes are no TAF");

    assert!(matches!(why, InspectError::Parse(_)), "{why:?}");
    // And a file that is not there at all is the other thing that stops a reading before it began.
    let missing = inspect(&dir.path().join("nowhere.taf")).expect_err("a file that is not there");
    assert!(matches!(missing, InspectError::Open(_)), "{missing:?}");
}
