//! The conversion a bare command line asks for: what was typed made into a job, the job run, and
//! what it came to said out loud.
//!
//! # What is said where
//!
//! The files that were written go to stdout, one per line, so a run can be read by whatever called
//! it. Everything else — the line a running conversion writes over, a chapter list longer than a
//! box plays, a cover that could not be written — goes to stderr, because none of it is the answer
//! to what was asked.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use taffle::{
    default_output_path, run_convert, ChapterError, ChapterMode, Conversion, ConvertError,
    ConvertJob, JobError, JobOutcome, Progress, SilenceOpts,
};

use crate::cli::ConvertArgs;
use crate::duration::{clock, RATE};

/// The chapters a Toniebox plays, as `FORMAT.md` states it from teddycloud's own limit.
///
/// A longer list is a warning and not a refusal: what a box does with the hundredth chapter is the
/// box's business, everything else that reads a TAF reads the whole of it, and a file that took an
/// hour to convert is not thrown away over a device that is not here.
const MAX_CHAPTERS: usize = 99;

/// Converts the files `args` names, and says what came of it.
///
/// # Errors
///
/// If the output is one of the inputs, or if the conversion itself failed — an input that could not
/// be read, a file that could not be written, a chapter list that is no plan.
pub fn run(args: ConvertArgs) -> Result<()> {
    let job = job(args)?;

    // The chapter count is said once, where it first stands: a plan somebody typed is settled in
    // front of the audio, so saying it there is a chance to stop the run rather than something
    // found out an hour of encoding later.
    let planned = planned_chapters(&job.options.chapter_mode);
    if let Some(chapters) = planned {
        warn_over_limit(chapters);
    }

    let mut line = ProgressLine::default();
    let outcome = run_convert(job, &mut |event| line.show(event));
    // Whatever is said next — the file that was written, or why it was not — begins on a line of
    // its own.
    line.finish();
    let outcome = outcome.map_err(in_clock_time)?;

    // A plan nobody typed is settled by the conversion, and this is where it stands: the chapters
    // the file holds.
    if planned.is_none() {
        warn_over_limit(outcome.report.chapters.len());
    }
    report(&outcome);

    Ok(())
}

/// The job `args` states, with the output resolved and held against the inputs.
///
/// # Errors
///
/// If the output is one of the inputs.
fn job(args: ConvertArgs) -> Result<ConvertJob> {
    let ConvertArgs {
        inputs,
        output,
        skip_leading,
        trim_pause_leading,
        trim_pause_each_chapter,
        add_pause_leading,
        add_pause_each_chapter,
        chapters,
        no_cover,
    } = args;

    let output = output.or_else(|| inputs.first().map(|first| default_output_path(first)));
    refuse_collision(&inputs, output.as_ref())?;

    Ok(ConvertJob {
        inputs,
        output,
        options: Conversion {
            chapter_mode: match chapters {
                Some(offsets) => {
                    ChapterMode::Explicit(offsets.iter().map(|at| at.to_samples_48k()).collect())
                }
                None => ChapterMode::Auto,
            },
            silence: SilenceOpts {
                skip_leading: skip_leading.to_samples_48k(),
                trim_leading: trim_pause_leading,
                trim_each_chapter: trim_pause_each_chapter,
                add_pause_leading: add_pause_leading.to_samples_48k(),
                add_pause_each_chapter: add_pause_each_chapter.to_samples_48k(),
            },
            // Nothing on the command line states how many encoders to run, so a conversion takes
            // the machine as it finds it.
            workers: None,
        },
        write_cover: !no_cover,
    })
}

/// Refuses a conversion that would write into a file it is reading.
///
/// The output is created by emptying whatever is at its name, and an input that *is* that file
/// would then be read out of the file being written. Both lists are in hand before anything is
/// opened, so this is settled while there is still nothing on the disk to undo.
///
/// The paths are compared as they were typed: two names for one file — a symbolic link, a
/// directory reached another way — are two names here, and the conversion runs.
///
/// # Errors
///
/// If any input is the output.
fn refuse_collision(inputs: &[PathBuf], output: Option<&PathBuf>) -> Result<()> {
    let Some(clash) = inputs.iter().find(|input| Some(*input) == output) else {
        return Ok(());
    };

    bail!(
        "the output {} is one of the inputs: converting it would write over the audio being read",
        clash.display()
    );
}

/// A chapter that lies past the end of the audio, said in the clock it was typed in.
///
/// The engine counts in frames, because frames are what it works in, and it says so: *explicit
/// chapter at 960000 beyond total length 96000*. What a person typed was `0:20`, and what they read
/// back should be the same thing — so the frontend that took the time in puts the times in front of
/// the engine's own line rather than in the place of it, since the frames are the exact answer and
/// the clock is the legible one.
///
/// Every other failure is handed on as it stands.
fn in_clock_time(error: JobError) -> anyhow::Error {
    // The two layers in between state nothing of their own — they are `transparent`, so the chain
    // renders as the chapter error alone — and this is where they are seen through.
    let out_of_range = match &error {
        JobError::Convert(ConvertError::Chapters(ChapterError::OutOfRange { offset, total })) => {
            Some((*offset, *total))
        }
        _ => None,
    };
    let error = anyhow::Error::new(error);

    let Some((offset, total)) = out_of_range else {
        return error;
    };

    error.context(format!(
        "the chapter at {} is past the end of the audio, which runs {}",
        at_clock(offset),
        at_clock(total)
    ))
}

/// Where `frames` of a conversion's audio lie, on a clock.
fn at_clock(frames: u64) -> String {
    clock(Duration::from_secs(frames / u64::from(RATE)))
}

/// How many chapters a stated plan comes to, where the caller stated one.
///
/// A TAF's first chapter begins where its audio does, so a plan that does not begin at the start of
/// it has that chapter put in front of it — one chapter more than was typed.
fn planned_chapters(mode: &ChapterMode) -> Option<usize> {
    let ChapterMode::Explicit(offsets) = mode else {
        return None;
    };
    let opening = usize::from(offsets.first() != Some(&0));

    Some(offsets.len() + opening)
}

/// Says so where `chapters` is more of them than a box plays.
fn warn_over_limit(chapters: usize) {
    if chapters > MAX_CHAPTERS {
        eprintln!("warning: {chapters} chapters is more than the {MAX_CHAPTERS} a Toniebox plays");
    }
}

/// What the run came to: the files it wrote, and the cover it could not write.
fn report(outcome: &JobOutcome) {
    let chapters = outcome.report.chapters.len();
    let plural = if chapters == 1 { "chapter" } else { "chapters" };

    println!(
        "wrote {} ({}, {chapters} {plural})",
        outcome.taf_path.display(),
        clock(outcome.report.duration),
    );
    if let Some(cover) = &outcome.cover_path {
        println!("wrote {}", cover.display());
    }
    // A cover is a file beside the file, and a book that converted is converted without it.
    if let Some(why) = &outcome.cover_error {
        eprintln!("warning: no cover was written: {why}");
    }
}

/// The one line a running conversion writes over: how much audio has gone into the file.
#[derive(Default)]
struct ProgressLine {
    /// The seconds last written, so a second is written once however many blocks it took — and so
    /// that a run that reported nothing leaves no line behind to end.
    shown: Option<u64>,
}

impl ProgressLine {
    /// Shows what the conversion is doing, where there is anything new to show.
    fn show(&mut self, event: Progress) {
        // Everything else leaves this line standing still: it says how much audio is in, and
        // neither the input being read nor the file being closed puts any in — nor does whatever
        // the engine comes to state next, until this line is taught to state it.
        if let Progress::Encoded { samples_done } = event {
            self.encoded(samples_done);
        }
    }

    /// Writes the seconds `samples` come to over the line, where that is not what it already says.
    fn encoded(&mut self, samples: u64) {
        let seconds = samples / u64::from(RATE);
        if self.shown == Some(seconds) {
            return;
        }
        self.shown = Some(seconds);

        // Stderr is unbuffered, so the line is on the screen as it is written.
        eprint!("\r{seconds}s encoded");
    }

    /// Ends the line, where anything was ever written on it, so that what is said next begins on
    /// one of its own.
    fn finish(&self) {
        if self.shown.is_some() {
            eprintln!();
        }
    }
}
