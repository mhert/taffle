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

// The chrome that calls all of this arrives with the dialog it serves, and `pub` keeps nothing
// alive in a binary crate — so outside the tests below, every item here is still unreached. Stated
// as an expectation rather than an allow: the day the chrome does call it, the expectation goes
// unfulfilled and the build says so, and this attribute leaves with the wait it stands for.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the chrome that calls this comes with the dialog")
)]

use std::path::PathBuf;

use taffle::duration::Seconds;
use taffle::{
    default_output_path, planned_chapters, ChapterMode, Conversion, ConvertJob, SilenceOpts,
    MAX_CHAPTERS,
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
    /// Whether the silence the first chapter begins with is dropped.
    pub trim_leading: bool,
    /// Whether the silence every chapter begins with is dropped.
    pub trim_each_chapter: bool,
    /// How much silence goes in front of the first chapter. Empty is none of it.
    pub add_pause_leading_text: String,
    /// How much silence goes in front of every chapter. Empty is none of it.
    pub add_pause_each_text: String,
    /// Whether the cover art an input carries is written beside the TAF.
    pub extract_cover: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            output_text: String::new(),
            chapters_text: String::new(),
            skip_leading_text: String::new(),
            trim_leading: false,
            trim_each_chapter: false,
            add_pause_leading_text: String::new(),
            add_pause_each_text: String::new(),
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
pub fn capture(panel: &Panel) -> Result<BookPlan, CaptureError> {
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
        default_output_path(first)
    } else {
        PathBuf::from(&panel.output_text)
    };

    let job = ConvertJob {
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
            // Nothing in the panel states how many encoders to run, so a conversion takes the
            // machine as it finds it.
            workers: None,
        },
        write_cover: panel.extract_cover,
    };

    Ok(BookPlan {
        title,
        panel: panel.clone(),
        job,
    })
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

    use taffle::{ChapterMode, MAX_CHAPTERS};

    use super::{capture, chapter_warning, CaptureError, Panel};

    /// A panel holding `files` and nothing typed into any of its fields.
    fn panel(files: &[&str]) -> Panel {
        Panel {
            files: files.iter().map(PathBuf::from).collect(),
            ..Panel::default()
        }
    }

    #[test]
    fn an_empty_panel_with_files_is_the_engine_defaults() {
        let plan = capture(&panel(&["a/01.mp3", "a/02.mp3"])).expect("a plan");
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
        let plan = capture(&p).expect("a plan");
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
        let error = capture(&p).expect_err("no duration");
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
        let error = capture(&p).expect_err("no chapter list");
        assert_eq!(
            error.to_string(),
            "the chapter list holds 'twelve', which is no duration"
        );
    }

    #[test]
    fn no_files_is_no_plan() {
        assert!(matches!(capture(&panel(&[])), Err(CaptureError::NoFiles)));
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
}
