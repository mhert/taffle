//! The options panel as it is typed, and the conversion it comes to.
//!
//! A panel holds text for as long as somebody is typing into it, and a conversion is made of paths
//! and frames. [`capture`] is where the one becomes the other, and it happens once per book: where
//! the book is put into the batch, or where Convert takes the book still being edited. So nothing
//! that cannot be read enters the queue, and no book fails an hour into a batch over a typo.
//!
//! What was typed is kept beside what it was read as, so a queued book opened again shows `12:34`
//! rather than the 36 192 000 frames it came to.
//!
//! Every duration here is read by [`Seconds`], which is the grammar the command line takes its own
//! times in: one grammar for the whole of taffle, rather than one per frontend.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use taffle::duration::{clock, Seconds, RATE};
use taffle::{
    default_output_path, planned_chapters, ChapterMode, Conversion, ConvertJob, Layout, PiecePlan,
    SilenceOpts, MAX_CHAPTERS,
};

/// The options panel as it stands: text where a person types, switches where they switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panel {
    /// The files to convert, in the order they play — each of them begins a chapter.
    pub files: Vec<PathBuf>,
    /// Where the TAF goes. Empty is the name derived from the first input.
    pub output_text: String,
    /// The chapter marks that override whatever the inputs carry, separated by commas. Empty
    /// leaves the chapters to the conversion.
    pub chapters_text: String,
    /// How much is dropped from the very start. Empty is none of it.
    pub skip_leading_text: String,
    /// How much is dropped from the very end. Empty is none of it.
    pub skip_trailing_text: String,
    /// Whether the silence the first chapter begins with is dropped.
    pub trim_leading: bool,
    /// Whether the silence every chapter begins with is dropped.
    pub trim_each_chapter: bool,
    /// How much silence goes in front of the first chapter. Empty is none of it.
    pub add_pause_leading_text: String,
    /// How much silence goes in front of every chapter. Empty is none of it.
    pub add_pause_each_text: String,
    /// How many files the book is written as. Empty is one.
    pub pieces_text: String,
    /// Whether the cover art an input carries is written beside the TAF.
    pub extract_cover: bool,
}

/// How much a fresh panel drops from the very start: Audible's spoken intro.
const AUDIBLE_INTRO: &str = "4.0";

/// How much a fresh panel drops from the very end: Audible's spoken outro.
const AUDIBLE_OUTRO: &str = "2.45";

impl Default for Panel {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            output_text: String::new(),
            chapters_text: String::new(),
            // Most books converted here come out of Audible, which puts the same spoken intro
            // in front of every book and the same spoken outro behind it. The intro ends 3.67 s
            // in and no book begins before 4.24 s; the outro begins at most 2.38 s before the
            // end and no book ends later than 2.55 s before it. So these two take both off and
            // leave every book whole — and a book that is no Audible one is a field to empty.
            skip_leading_text: String::from(AUDIBLE_INTRO),
            skip_trailing_text: String::from(AUDIBLE_OUTRO),
            trim_leading: false,
            trim_each_chapter: false,
            add_pause_leading_text: String::new(),
            add_pause_each_text: String::new(),
            pieces_text: String::new(),
            // A cover is extracted unless somebody switches it off, which is what the command
            // line's own `--no-cover` default is. Written out rather than derived from the type,
            // because a derived default is `false` — and this is the panel a book is added from
            // and the one it is reset to, so deriving it would quietly stop extracting covers.
            extract_cover: true,
        }
    }
}

/// A book as it is going to be converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookPlan {
    /// What the book is called in the queue: the first input's name without its extension.
    pub title: String,
    /// The panel this was captured from, kept so that a book opened again shows what was typed
    /// rather than what it was read as.
    pub panel: Panel,
    /// The conversion the panel came to.
    pub job: ConvertJob,
    /// Where the book is cut, where it is written as more than one file.
    pub pieces: Option<PiecePlan>,
}

/// Why what was typed is no conversion.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The panel names no file at all.
    #[error("there is nothing to convert: the book holds no files")]
    NoFiles,
    /// One of the duration fields holds something that is no duration.
    #[error("the {field} time '{text}' is no duration")]
    BadDuration {
        /// The panel field it was typed into, as the panel names it.
        field: &'static str,
        /// What was typed there.
        text: String,
    },
    /// One entry of the chapter list is no duration.
    #[error("the chapter list holds '{text}', which is no duration")]
    BadChapterEntry {
        /// The entry that is no time, as it was typed.
        text: String,
    },
    /// The pieces field holds something that is no count of files.
    #[error("the pieces count '{text}' is no whole number of at least 1")]
    BadPieces {
        /// What was typed there.
        text: String,
    },
    /// The book cannot be cut into the pieces that were typed.
    #[error(transparent)]
    Pieces(#[from] taffle::PlanError),
}

/// The conversion `panel` states, read out of what was typed into it.
///
/// The output is resolved here rather than where the conversion runs, so a book that is waiting
/// already states where it will land.
///
/// # Errors
///
/// - [`CaptureError::NoFiles`] where the panel names no file to convert.
/// - [`CaptureError::BadDuration`] where a duration field holds something that is no duration,
///   naming the field and echoing what was typed.
/// - [`CaptureError::BadChapterEntry`] where an entry of the chapter list is no duration.
/// - [`CaptureError::BadPieces`] where the pieces field holds no whole number of at least 1.
/// - [`CaptureError::Pieces`] where the book cannot be cut into the pieces that were typed.
///
/// `layouts` is index-aligned with the files of the panel and read by whoever holds the panel. It
/// is only looked at where more than one piece is typed.
pub fn capture(panel: &Panel, layouts: &[Option<Layout>]) -> Result<BookPlan, CaptureError> {
    let Some(first) = panel.files.first() else {
        return Err(CaptureError::NoFiles);
    };
    // A file that was picked has a name; one that somehow has none leaves the row unnamed rather
    // than refusing a book that would convert perfectly well.
    let title = first
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let output = if panel.output_text.is_empty() {
        // This is the name the output field stands empty for; see `derived_output`.
        default_output_path(first)
    } else {
        PathBuf::from(&panel.output_text)
    };

    let mut job = ConvertJob {
        inputs: panel.files.clone(),
        output: Some(output),
        options: Conversion {
            chapter_mode: chapter_mode(&panel.chapters_text)?,
            silence: SilenceOpts {
                skip_leading: samples(&panel.skip_leading_text, "skip leading")?,
                trim_leading: panel.trim_leading,
                trim_each_chapter: panel.trim_each_chapter,
                add_pause_leading: samples(&panel.add_pause_leading_text, "add pause leading")?,
                add_pause_each_chapter: samples(
                    &panel.add_pause_each_text,
                    "add pause each chapter",
                )?,
            },
            skip_trailing: samples(&panel.skip_trailing_text, "skip trailing")?,
            // Nothing in the panel states how many encoders to run, so a conversion takes the
            // machine as it finds it.
            workers: None,
        },
        write_cover: panel.extract_cover,
        piece_starts: Vec::new(),
    };
    // A book in one file is planned by nothing: only a count above one reads what the files
    // state, so a file that states nothing still converts whole.
    let pieces = match piece_count(&panel.pieces_text)? {
        count if count.get() > 1 => {
            let plan = taffle::plan_pieces(&job, layouts, count)?;
            job.piece_starts = plan.starts();

            Some(plan)
        }
        _ => None,
    };

    Ok(BookPlan {
        title,
        panel: panel.clone(),
        job,
        pieces,
    })
}

/// How long the pieces of `plan` are stated to play, as the panel shows it under the count.
///
/// The lengths are what the files state about themselves and what is trimmed or put in is not in
/// them, which is what the sign in front says.
#[must_use]
pub fn pieces_preview(plan: &PiecePlan) -> String {
    let lengths: Vec<String> = plan
        .pieces
        .iter()
        .map(|piece| clock(Duration::from_secs(piece.frames / u64::from(RATE))))
        .collect();

    format!("≈ {}", lengths.join(" · "))
}

/// Says so where a piece of `plan` holds more chapters than a Toniebox plays: a box counts the
/// chapters of a file, and a piece is one.
#[must_use]
pub fn piece_warning(plan: &PiecePlan) -> Option<String> {
    let (at, piece) = plan
        .pieces
        .iter()
        .enumerate()
        .find(|(_, piece)| piece.chapters > MAX_CHAPTERS)?;

    Some(format!(
        "piece {} holds {} chapters, which is more than the {MAX_CHAPTERS} a Toniebox plays",
        at + 1,
        piece.chapters
    ))
}

/// How many files `text` asks for, where nothing typed is one.
fn piece_count(text: &str) -> Result<NonZeroUsize, CaptureError> {
    if text.is_empty() {
        return Ok(NonZeroUsize::MIN);
    }

    text.parse().map_err(|_| CaptureError::BadPieces {
        text: text.to_owned(),
    })
}

/// Where the book in `panel` goes while nothing is typed into its output field: the name derived
/// from the first input, which is exactly what [`capture`] resolves an empty field to — and nothing
/// at all where the panel holds no file to derive a name from.
///
/// The field shows this while it stands empty, so that what a conversion will write is read off the
/// panel rather than guessed at by whoever is looking at it.
#[must_use]
pub fn derived_output(panel: &Panel) -> Option<PathBuf> {
    panel.files.first().map(|first| default_output_path(first))
}

/// Says so where `mode` plans more chapters than a Toniebox plays, in the words the command line
/// says it in.
///
/// Nothing to say covers both a plan that fits and a plan nobody typed: what a conversion decides
/// for itself is counted where the file is written and not here.
#[must_use]
pub fn chapter_warning(mode: &ChapterMode) -> Option<String> {
    let chapters = planned_chapters(mode)?;
    if chapters <= MAX_CHAPTERS {
        return None;
    }

    Some(format!(
        "{chapters} chapters is more than the {MAX_CHAPTERS} a Toniebox plays"
    ))
}

/// The chapter plan `text` states: every time it names, in the order they were typed.
///
/// Nothing typed leaves the chapters to the conversion, which is what the command line does
/// without its chapter option.
fn chapter_mode(text: &str) -> Result<ChapterMode, CaptureError> {
    if text.is_empty() {
        return Ok(ChapterMode::Auto);
    }

    // The list is split on commas and every entry read exactly as it stands — the same grammar
    // the command line's own chapter list is read in, down to a stray space being refused rather
    // than guessed past.
    let offsets: Vec<u64> = text
        .split(',')
        .map(|entry| {
            entry
                .parse::<Seconds>()
                .map(Seconds::to_samples_48k)
                .map_err(|_| CaptureError::BadChapterEntry {
                    text: entry.to_owned(),
                })
        })
        .collect::<Result<_, _>>()?;

    Ok(ChapterMode::Explicit(offsets))
}

/// The frames `text` comes to, where `field` is the panel field it was typed into.
///
/// A field nobody typed into is no time at all, which is the command line's own default for every
/// one of them.
fn samples(text: &str, field: &'static str) -> Result<u64, CaptureError> {
    if text.is_empty() {
        return Ok(0);
    }

    text.parse::<Seconds>()
        .map(Seconds::to_samples_48k)
        .map_err(|_| CaptureError::BadDuration {
            field,
            text: text.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;

    use taffle::{ChapterMode, Layout, PiecePlan, PlannedPiece, MAX_CHAPTERS};

    use super::{
        capture, chapter_warning, derived_output, piece_warning, pieces_preview, CaptureError,
        Panel,
    };

    /// A panel holding `files` and nothing typed into any of its fields — the Audible skips a
    /// fresh panel starts with included, so that what a test types is all a conversion is
    /// asked for.
    fn panel(files: &[&str]) -> Panel {
        Panel {
            files: files.iter().map(PathBuf::from).collect(),
            skip_leading_text: String::new(),
            skip_trailing_text: String::new(),
            ..Panel::default()
        }
    }

    /// What a book of two chapters five seconds long each states about itself.
    fn two_chapters() -> Vec<Option<Layout>> {
        vec![Some(Layout {
            frames: 480_000,
            marks: vec![0, 240_000],
        })]
    }

    #[test]
    fn a_fresh_panel_drops_the_audible_intro_and_outro() {
        let fresh = Panel {
            files: vec![PathBuf::from("b.m4b")],
            ..Panel::default()
        };

        let plan = capture(&fresh, &[]).expect("a plan");

        // 4.0 s and 2.45 s, in the 48 kHz frames a conversion counts in — and what is shown in
        // the fields is what was typed there, so the panel says where the numbers came from.
        assert_eq!(plan.job.options.silence.skip_leading, 192_000);
        assert_eq!(plan.job.options.skip_trailing, 117_600);
        assert_eq!(fresh.skip_leading_text, "4.0");
        assert_eq!(fresh.skip_trailing_text, "2.45");
    }

    #[test]
    fn an_empty_panel_with_files_is_the_engine_defaults() {
        let plan = capture(&panel(&["a/01.mp3", "a/02.mp3"]), &[]).expect("a plan");
        assert_eq!(plan.title, "01");
        assert_eq!(
            plan.job.inputs,
            [PathBuf::from("a/01.mp3"), PathBuf::from("a/02.mp3")]
        );
        assert_eq!(plan.job.options, taffle::Conversion::default());
        assert!(plan.job.write_cover);
        assert_eq!(plan.job.output, Some(PathBuf::from("a/01.taf")));
    }

    #[test]
    fn every_typed_field_lands_in_the_job() {
        let mut p = panel(&["b.m4b"]);
        p.output_text = "out/b.taf".into();
        p.chapters_text = "0:00,12:34".into();
        p.skip_leading_text = "4.4".into();
        p.trim_leading = true;
        p.add_pause_each_text = "1".into();
        p.extract_cover = false;
        let plan = capture(&p, &[]).expect("a plan");
        assert_eq!(plan.job.output, Some(PathBuf::from("out/b.taf")));
        assert_eq!(
            plan.job.options.chapter_mode,
            ChapterMode::Explicit(vec![0, 754 * 48_000])
        );
        assert_eq!(plan.job.options.silence.skip_leading, 211_200);
        assert!(plan.job.options.silence.trim_leading);
        assert_eq!(plan.job.options.silence.add_pause_each_chapter, 48_000);
        assert!(!plan.job.write_cover);
        // What was typed is kept beside what it was read as, so opening the book again shows the
        // times rather than the frames they came to.
        assert_eq!(plan.panel, p);
    }

    #[test]
    fn what_is_no_duration_names_its_field() {
        let mut p = panel(&["b.m4b"]);
        p.skip_leading_text = "abc".into();
        let error = capture(&p, &[]).expect_err("no duration");
        assert!(matches!(
            error,
            CaptureError::BadDuration {
                field: "skip leading",
                ..
            }
        ));
        assert_eq!(
            error.to_string(),
            "the skip leading time 'abc' is no duration"
        );
    }

    #[test]
    fn a_chapter_that_is_no_time_is_named_as_it_was_typed() {
        let mut p = panel(&["b.m4b"]);
        p.chapters_text = "0:00,twelve".into();
        let error = capture(&p, &[]).expect_err("no chapter list");
        assert_eq!(
            error.to_string(),
            "the chapter list holds 'twelve', which is no duration"
        );
    }

    #[test]
    fn an_empty_output_field_stands_for_the_name_a_capture_resolves() {
        let p = panel(&["a/01.mp3", "a/02.mp3"]);
        // The field shows this as its placeholder, so it has to be the very path the book is
        // written to where nobody types one — held against the capture rather than restated.
        assert_eq!(derived_output(&p), Some(PathBuf::from("a/01.taf")));
        assert_eq!(
            capture(&p, &[]).expect("a plan").job.output,
            derived_output(&p)
        );
        // A book with no file derives no name, and the field stands for nothing.
        assert_eq!(derived_output(&panel(&[])), None);
    }

    #[test]
    fn no_files_is_no_plan() {
        assert!(matches!(
            capture(&panel(&[]), &[]),
            Err(CaptureError::NoFiles)
        ));
    }

    #[test]
    fn a_fresh_panel_extracts_the_cover() {
        // The command line writes the cover unless --no-cover says otherwise, and a panel starts
        // where the command line does — including every panel reset back to this one.
        assert!(Panel::default().extract_cover);
    }

    #[test]
    fn a_plan_longer_than_a_box_plays_says_so_the_way_the_command_line_does() {
        let marks = |count: u64| ChapterMode::Explicit((0..count).map(|at| at * 48_000).collect());

        // A plan nobody typed is the conversion's own, and there is nothing to count in front of
        // it; a plan that fits has nothing to say either.
        assert_eq!(chapter_warning(&ChapterMode::Auto), None);
        assert_eq!(chapter_warning(&marks(MAX_CHAPTERS as u64)), None);
        assert_eq!(
            chapter_warning(&marks(MAX_CHAPTERS as u64 + 1)).as_deref(),
            Some("100 chapters is more than the 99 a Toniebox plays")
        );
    }

    #[test]
    fn the_end_that_is_skipped_lands_in_the_job() {
        let mut p = panel(&["b.m4b"]);
        p.skip_trailing_text = "2.5".into();

        let plan = capture(&p, &[]).expect("a plan");

        assert_eq!(plan.job.options.skip_trailing, 120_000);
        assert_eq!(plan.panel, p);
    }

    #[test]
    fn a_book_typed_into_pieces_is_planned_from_what_its_files_state() {
        let mut p = panel(&["b.m4b"]);
        p.pieces_text = "2".into();

        let plan = capture(&p, &two_chapters()).expect("a plan");

        assert_eq!(plan.job.piece_starts, [1]);
        assert_eq!(
            plan.pieces.as_ref().map(pieces_preview).as_deref(),
            Some("≈ 0:05 · 0:05")
        );
    }

    #[test]
    fn no_count_and_a_count_of_one_are_the_book_in_one_file() {
        for text in ["", "1"] {
            let mut p = panel(&["b.m4b"]);
            p.pieces_text = text.into();

            // No layout is asked for where nothing is planned, so a file that states none is no
            // obstacle to a book in one file.
            let plan = capture(&p, &[None]).expect("a plan");

            assert!(plan.job.piece_starts.is_empty(), "{text:?}");
            assert!(plan.pieces.is_none(), "{text:?}");
        }
    }

    #[test]
    fn a_pieces_field_that_holds_no_count_is_refused() {
        for text in ["0", "abc", "-2", "1.5"] {
            let mut p = panel(&["b.m4b"]);
            p.pieces_text = text.into();

            let error = capture(&p, &two_chapters()).expect_err("no count");

            assert_eq!(
                error.to_string(),
                format!("the pieces count '{text}' is no whole number of at least 1")
            );
        }
    }

    #[test]
    fn a_book_that_cannot_be_cut_says_why_in_the_words_the_command_line_says_it_in() {
        let mut p = panel(&["x/b.m4b"]);
        p.pieces_text = "3".into();

        assert_eq!(
            capture(&p, &two_chapters())
                .expect_err("too few")
                .to_string(),
            "3 pieces asked for, but the book has 2 chapters"
        );
        assert_eq!(
            capture(&p, &[None]).expect_err("no length").to_string(),
            "no length could be read off x/b.m4b, so the pieces cannot be planned"
        );
    }

    #[test]
    fn a_piece_longer_than_a_box_plays_says_which_one() {
        let piece = |chapters| PlannedPiece {
            first_chapter: 0,
            chapters,
            frames: 0,
        };
        let fits = PiecePlan {
            pieces: vec![piece(MAX_CHAPTERS), piece(1)],
        };
        let over = PiecePlan {
            pieces: vec![piece(3), piece(MAX_CHAPTERS + 1)],
        };

        assert_eq!(piece_warning(&fits), None);
        assert_eq!(
            piece_warning(&over).as_deref(),
            Some("piece 2 holds 100 chapters, which is more than the 99 a Toniebox plays")
        );
    }
}
