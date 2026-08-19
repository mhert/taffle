//! The chapter marks an MP4 keeps in its `chpl` atom, read straight out of the file.
//!
//! # Why this is not symphonia's job
//!
//! symphonia has a place for chapter marks — [`FormatReader::cues`] — and its MP4 demuxer never
//! puts anything in it: `IsoMp4Reader` is built with an empty cue list, and the `udta` atom it
//! reads looks at nothing but `meta`. Both m4b files this was written against were probed with it
//! and report `cues: 0`, while both carry their chapters plainly:
//!
//! - the fixture, whose `moov/udta/chpl` is 62 bytes stating 0 s "Anfang" and 5 s
//!   "Möhrchen macht Pause";
//! - a 61 MB audiobook, whose `chpl` is 312 bytes stating all sixteen of its chapters with the
//!   same timestamps its chapter *track* states.
//!
//! So the marks are read here. `chpl` is Nero's flat list of them and the smallest thing that
//! answers the question: a walk over three nested atoms and a run of fixed-shape entries, no
//! sample tables, no second track to resolve.
//!
//! # What this does not read
//!
//! `QuickTime`'s other way of stating chapters is a whole text track that the audio track points at
//! with a `chap` reference, and reading *that* means walking a sample table, finding its chunks in
//! `mdat` and decoding a text sample per mark. Both files above carry one alongside their `chpl`
//! and agree with it to the millisecond, so nothing is lost by leaving it alone — but a file that
//! carries only the track, which some encoders write, states no chapters as far as this is
//! concerned.
//!
//! `chpl` also counts its entries in a single byte, so a book of more than 255 chapters states the
//! first 255 here and the rest only in its track — one book on the machine this was written on has
//! 332 chapters and a `chpl` of 255. Neither limit reaches what a TAF carries: a Toniebox stops at
//! 99 chapters, so a book of that many is being cut down long before this is what loses marks.
//!
//! # Nothing here fails a conversion
//!
//! Chapter marks are what a file says about itself, and a file that says it badly still plays. So
//! every way this can go wrong — bytes that are not an MP4 at all, an atom that ends early, a
//! title that is not the UTF-8 the format calls for — comes back as marks that were not found,
//! and the demuxer behind this gets its turn at the same bytes either way. The one failure that
//! *is* reported is being unable to put the input back where it was found, because everything
//! after this reads from there.
//!
//! [`FormatReader::cues`]: symphonia::core::formats::FormatReader::cues

use std::io::{self, SeekFrom};
use std::ops::Range;

use symphonia::core::io::MediaSource;

/// A chapter mark as an MP4 states it.
pub(super) struct Mp4Chapter {
    /// Where the chapter starts, counted from the start of the presentation in the hundreds of
    /// nanoseconds `chpl` measures in.
    pub start_100ns: u64,
    /// What the atom called the chapter, when it called it anything.
    pub title: Option<String>,
}

/// Every chapter mark the input states, and the input rewound to where it was found.
///
/// An input that cannot seek is left alone entirely and states no chapters: reading its atoms
/// would eat the bytes the demuxer behind this needs, and there would be no giving them back.
///
/// # Errors
///
/// [`io::Error`] when the input cannot be rewound after being read.
pub(super) fn read(source: &mut dyn MediaSource) -> io::Result<Vec<Mp4Chapter>> {
    if !source.is_seekable() {
        return Ok(Vec::new());
    }

    let chapters = scan(source).unwrap_or_default();
    source.seek(SeekFrom::Start(0))?;

    Ok(chapters)
}

/// The chapter marks in `moov/udta/chpl`, or none when any of that is missing.
fn scan(source: &mut dyn MediaSource) -> io::Result<Vec<Mp4Chapter>> {
    let file = 0..source.byte_len().unwrap_or(u64::MAX);

    // An MP4 opens with a `ftyp` atom. Anything that does not is a format this knows nothing
    // about, and its bytes are not walked as if they were atoms.
    match atom_at(source, file.start, file.end)? {
        Some(first) if first.kind == *b"ftyp" => {}
        _ => return Ok(Vec::new()),
    }

    let Some(moov) = find(source, *b"moov", file)? else {
        return Ok(Vec::new());
    };
    let Some(udta) = find(source, *b"udta", moov)? else {
        return Ok(Vec::new());
    };
    let Some(chpl) = find(source, *b"chpl", udta)? else {
        return Ok(Vec::new());
    };

    chapters(source, chpl)
}

/// What one atom is, and what lies inside it.
struct Atom {
    /// The four bytes naming the atom.
    kind: [u8; 4],
    /// Where the atom's contents begin and where the whole atom ends.
    contents: Range<u64>,
}

/// The contents of the first atom of `kind` among those in `within`, if it holds one.
///
/// The walk steps from atom to atom by their stated lengths and stops at the end of the range it
/// was given, so an atom that overstates its length cannot walk out of its parent. It also stops
/// wherever a step would not move it forward: every atom is at least as long as its own header, so
/// that cannot happen — and a walk over a file that says otherwise ends rather than never ending.
fn find(
    source: &mut dyn MediaSource,
    kind: [u8; 4],
    within: Range<u64>,
) -> io::Result<Option<Range<u64>>> {
    let mut pos = within.start;

    while let Some(atom) = atom_at(source, pos, within.end)? {
        if atom.kind == kind {
            return Ok(Some(atom.contents));
        }
        if atom.contents.end <= pos {
            break;
        }
        pos = atom.contents.end;
    }

    Ok(None)
}

/// The atom beginning at `pos`, or `None` when no whole atom begins there.
///
/// An atom opens with its own length and the four bytes naming it. A length of 1 means the real,
/// 64-bit length follows the name; a length smaller than the header it was read from describes an
/// atom that cannot be stepped over, and ends the walk it was found in — as does a length of 0,
/// which means "to the end of the file" and is a shape nothing this reads is written in.
fn atom_at(source: &mut dyn MediaSource, pos: u64, end: u64) -> io::Result<Option<Atom>> {
    /// Four bytes of length and four naming the atom.
    const HEADER: u64 = 8;
    /// The same, and the eight bytes of length that a stated length of one puts behind them.
    const LONG_HEADER: u64 = 16;

    if pos.saturating_add(HEADER) > end {
        return Ok(None);
    }

    source.seek(SeekFrom::Start(pos))?;
    let mut length = [0_u8; 4];
    let mut kind = [0_u8; 4];
    source.read_exact(&mut length)?;
    source.read_exact(&mut kind)?;

    let (length, header) = match u32::from_be_bytes(length) {
        1 => (read_u64(source)?, LONG_HEADER),
        length => (u64::from(length), HEADER),
    };
    if length < header || pos.saturating_add(length) > end {
        return Ok(None);
    }

    Ok(Some(Atom {
        kind,
        contents: pos + header..pos + length,
    }))
}

/// The chapter marks a `chpl` atom's contents state.
///
/// The atom is a versioned one: a version byte and three of flags, and from version 1 on four
/// further bytes that no description of the format gives a meaning to. Behind them stands the
/// number of marks in a single byte, and then that many entries of a start time, the length of a
/// title in bytes, and the title.
///
/// An atom whose entries reach past its own end was not written the way it says it was, and none
/// of what was read out of it is passed on.
fn chapters(source: &mut dyn MediaSource, within: Range<u64>) -> io::Result<Vec<Mp4Chapter>> {
    source.seek(SeekFrom::Start(within.start))?;

    let mut versioning = [0_u8; 4];
    source.read_exact(&mut versioning)?;
    let [version, ..] = versioning;
    if version > 0 {
        source.read_exact(&mut [0_u8; 4])?;
    }

    let count = read_u8(source)?;
    let mut chapters = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let start_100ns = read_u64(source)?;
        let mut title = vec![0_u8; usize::from(read_u8(source)?)];
        source.read_exact(&mut title)?;
        chapters.push(Mp4Chapter {
            start_100ns,
            title: title_of(title),
        });
    }

    if source.stream_position()? > within.end {
        return Ok(Vec::new());
    }

    Ok(chapters)
}

/// A chapter's title, where a name it does not have is one it was not given.
///
/// `chpl` states titles in UTF-8. Bytes that are not that are a title this cannot pass on, and
/// neither is one of no bytes at all — an entry may carry a title without stating a name.
fn title_of(title: Vec<u8>) -> Option<String> {
    String::from_utf8(title).ok().filter(|it| !it.is_empty())
}

/// The next byte.
fn read_u8(source: &mut dyn MediaSource) -> io::Result<u8> {
    let mut byte = [0_u8; 1];
    source.read_exact(&mut byte)?;

    Ok(u8::from_be_bytes(byte))
}

/// The next eight bytes, most significant first, as every number in an MP4 is written.
fn read_u64(source: &mut dyn MediaSource) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    source.read_exact(&mut bytes)?;

    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io::{self, Cursor, Read, Seek, SeekFrom};

    use symphonia::core::io::{MediaSource, ReadOnlySource};

    use super::{read, Mp4Chapter};

    /// An atom of `kind` around `body`.
    fn atom(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut atom = Vec::with_capacity(8 + body.len());
        let length = u32::try_from(8 + body.len()).unwrap();
        atom.extend_from_slice(&length.to_be_bytes());
        atom.extend_from_slice(&kind);
        atom.extend_from_slice(body);

        atom
    }

    /// An atom of `kind` around `body`, written with the 64-bit length a long one states.
    fn long_atom(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut atom = 1_u32.to_be_bytes().to_vec();
        atom.extend_from_slice(&kind);
        atom.extend_from_slice(&u64::try_from(16 + body.len()).unwrap().to_be_bytes());
        atom.extend_from_slice(body);

        atom
    }

    /// One `chpl` entry: a start time in hundreds of nanoseconds and a title.
    fn entry(start_100ns: u64, title: &[u8]) -> Vec<u8> {
        let mut entry = start_100ns.to_be_bytes().to_vec();
        entry.push(u8::try_from(title.len()).unwrap());
        entry.extend_from_slice(title);

        entry
    }

    /// A `chpl` atom of `version` stating `entries`, whose count it may misstate.
    fn chpl(version: u8, count: u8, entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        if version > 0 {
            body.extend_from_slice(&[0; 4]);
        }
        body.push(count);
        for entry in entries {
            body.extend_from_slice(entry);
        }

        atom(*b"chpl", &body)
    }

    /// An MP4 of nothing but a `ftyp` atom and whatever `udta` holds.
    fn mp4(udta: &[u8]) -> Vec<u8> {
        let mut file = atom(*b"ftyp", b"M4A \0\0\0\0M4A mp42isom");
        file.extend_from_slice(&atom(*b"moov", &atom(*b"udta", udta)));

        file
    }

    /// The chapters read out of a file, which is expected to be rewound afterwards.
    fn chapters_of(file: Vec<u8>) -> Vec<Mp4Chapter> {
        let mut source = Cursor::new(file);

        let chapters = read(&mut source).expect("a cursor rewinds");

        assert_eq!(source.position(), 0, "the input was left where it was read");
        chapters
    }

    /// What the chapters read out of a file amount to.
    fn marks_of(file: Vec<u8>) -> Vec<(u64, Option<String>)> {
        chapters_of(file)
            .into_iter()
            .map(|chapter| (chapter.start_100ns, chapter.title))
            .collect()
    }

    #[test]
    fn reads_every_entry_of_a_chapter_list() {
        let file = mp4(&chpl(
            1,
            2,
            &[
                entry(0, b"Anfang"),
                entry(50_000_000, "Möhrchen".as_bytes()),
            ],
        ));

        let marks = marks_of(file);

        assert_eq!(
            marks,
            [
                (0, Some("Anfang".to_owned())),
                (50_000_000, Some("Möhrchen".to_owned())),
            ]
        );
    }

    #[test]
    fn a_chapter_list_of_the_first_version_states_four_bytes_fewer() {
        let file = mp4(&chpl(0, 1, &[entry(10_000_000, b"Eins")]));

        let marks = marks_of(file);

        assert_eq!(marks, [(10_000_000, Some("Eins".to_owned()))]);
    }

    #[test]
    fn a_title_of_no_bytes_is_a_chapter_without_a_name() {
        let file = mp4(&chpl(1, 1, &[entry(0, b"")]));

        let marks = marks_of(file);

        assert_eq!(marks, [(0, None)]);
    }

    #[test]
    fn a_title_that_is_not_utf_8_is_a_chapter_without_a_name() {
        // The one byte that no UTF-8 sequence begins with.
        let file = mp4(&chpl(1, 1, &[entry(0, &[0xff])]));

        let marks = marks_of(file);

        assert_eq!(marks, [(0, None)]);
    }

    #[test]
    fn an_atom_of_a_64_bit_length_is_stepped_over_and_walked_into_like_any_other() {
        // Something to step over, and the user data holding it written the same way, so that the
        // list inside is only found by a walk that knows where a longer header ends.
        let mut udta = long_atom(*b"free", &[0; 8]);
        udta.extend_from_slice(&chpl(1, 1, &[entry(0, b"Hinter free")]));

        let mut file = atom(*b"ftyp", b"M4A ");
        file.extend_from_slice(&atom(*b"moov", &long_atom(*b"udta", &udta)));

        let marks = marks_of(file);

        assert_eq!(marks, [(0, Some("Hinter free".to_owned()))]);
    }

    #[test]
    fn a_chapter_list_that_reaches_past_its_own_atom_states_nothing() {
        // A count of two over an atom holding one, with an atom behind it to read into.
        let mut udta = chpl(1, 2, &[entry(0, b"Eins")]);
        udta.extend_from_slice(&atom(*b"free", &[0; 32]));

        let marks = marks_of(mp4(&udta));

        assert!(marks.is_empty(), "read {marks:?} out of a truncated list");
    }

    #[test]
    fn a_chapter_list_that_runs_out_mid_entry_states_nothing() {
        let mut chpl = chpl(1, 1, &[entry(0, b"Eins")]);
        chpl.truncate(chpl.len() - 2);

        let marks = marks_of(mp4(&chpl));

        assert!(marks.is_empty(), "read {marks:?} out of a broken list");
    }

    #[test]
    fn bytes_that_do_not_open_with_a_file_type_are_not_walked() {
        let mut riff = b"RIFF\0\0\0\0WAVE".to_vec();
        riff.extend_from_slice(&mp4(&chpl(1, 1, &[entry(0, b"Eins")])));

        let marks = marks_of(riff);

        assert!(marks.is_empty(), "walked a RIFF file and found {marks:?}");

        // QuickTime allowed a file to open with the movie itself. This does not read those, and
        // neither does the demuxer behind it.
        let movie_first = atom(*b"moov", &atom(*b"udta", &chpl(1, 1, &[entry(0, b"Eins")])));

        let marks = marks_of(movie_first);

        assert!(
            marks.is_empty(),
            "walked a headless movie and found {marks:?}"
        );
    }

    #[test]
    fn an_mp4_of_no_chapter_list_states_no_chapters() {
        // In turn: user data holding something else, no movie at all, no user data in the movie,
        // and user data holding nothing.
        assert!(marks_of(mp4(&atom(*b"free", &[0; 8]))).is_empty());
        assert!(marks_of(atom(*b"ftyp", b"M4A ")).is_empty());

        let mut without_udta = atom(*b"ftyp", b"M4A ");
        without_udta.extend_from_slice(&atom(*b"moov", &atom(*b"free", &[0; 8])));
        assert!(marks_of(without_udta).is_empty());

        assert!(marks_of(mp4(&[])).is_empty());
    }

    #[test]
    fn an_atom_that_states_a_length_of_its_own_header_or_less_ends_the_walk() {
        // Four bytes of length and nothing else, and behind them — exactly where a walk that
        // stepped by that length would land — a chapter list nothing should reach.
        let mut udta = 4_u32.to_be_bytes().to_vec();
        udta.extend_from_slice(&chpl(1, 1, &[entry(0, b"Hinter vier Bytes")]));

        let marks = marks_of(mp4(&udta));

        assert!(
            marks.is_empty(),
            "walked past a length of 4 and found {marks:?}"
        );
    }

    #[test]
    fn an_atom_that_states_a_length_past_its_parent_ends_the_walk() {
        let mut udta = u32::MAX.to_be_bytes().to_vec();
        udta.extend_from_slice(b"free");
        udta.extend_from_slice(&chpl(1, 1, &[entry(0, b"Hinter free")]));

        let marks = marks_of(mp4(&udta));

        assert!(
            marks.is_empty(),
            "walked past the file's end and found {marks:?}"
        );
    }

    #[test]
    #[ignore = "reads an audiobook that only exists on the machine this crate is written on"]
    fn a_real_audiobook_states_every_one_of_its_chapters() {
        let path = "/home/mhert/OpenAudible/books/\
                    Grimm und Möhrchen machen Pause von zu Hause (Teil 3).m4b";
        let mut book = std::fs::File::open(path).expect("the book is on this machine");

        let chapters = read(&mut book).expect("a file rewinds");

        let titles: Vec<Option<&str>> = chapters
            .iter()
            .map(|chapter| chapter.title.as_deref())
            .collect();
        assert_eq!(titles.len(), 16);
        assert_eq!(titles.first(), Some(&Some("Kapitel 1")));
        assert_eq!(titles.last(), Some(&Some("Kapitel 16")));
        // The book's second chapter starts 195.395 s in, which its chapter track states to the
        // same millisecond: 1 953 950 000 hundreds of nanoseconds.
        assert_eq!(
            chapters.get(1).map(|chapter| chapter.start_100ns),
            Some(1_953_950_000)
        );
    }

    #[test]
    fn an_input_that_cannot_seek_states_no_chapters() {
        let mp4 = mp4(&chpl(1, 1, &[entry(0, b"Eins")]));

        let chapters =
            read(&mut ReadOnlySource::new(Cursor::new(mp4))).expect("nothing was read to rewind");

        assert!(chapters.is_empty());
    }

    #[test]
    fn an_input_that_cannot_be_put_back_where_it_was_found_is_a_failure() {
        let read = read(&mut Broken);

        assert!(
            read.is_err(),
            "an input that cannot be rewound came back with chapters"
        );
    }

    /// A source that says it can be seeked and then cannot: what a file that goes away between
    /// being opened and being read looks like from here.
    struct Broken;

    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::NotConnected, "gone"))
        }
    }

    impl Seek for Broken {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Err(io::Error::new(io::ErrorKind::NotConnected, "gone"))
        }
    }

    impl MediaSource for Broken {
        fn is_seekable(&self) -> bool {
            true
        }

        fn byte_len(&self) -> Option<u64> {
            None
        }
    }
}
