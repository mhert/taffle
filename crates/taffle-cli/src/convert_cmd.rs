//! The conversion a bare command line asks for: what was typed made into a job, the job run, and
//! what it came to said out loud.
//!
//! # What is said where
//!
//! The files that were written go to stdout, one per line, so a run can be read by whatever called
//! it. Everything else — the line a running conversion writes over, a chapter list longer than a
//! box plays, the plan of a run cut into pieces, a cover that could not be written — goes to
//! stderr, because none of it is the answer to what was asked.

use std::time::Duration;

use anyhow::Result;
use taffle::duration::{clock, RATE};
use taffle::{
    default_output_path, fewer_pieces, output_paths, plan_cuts, plan_pieces, planned_chapters,
    probe_layout, refuse_collisions, run_convert, ChapterError, ChapterMode, Conversion,
    ConvertError, ConvertJob, JobError, JobOutcome, Layout, PiecePlan, PlanError, Progress,
    SilenceOpts, MAX_CHAPTERS,
};

use crate::cli::ConvertArgs;

/// Converts the files `args` names, and says what came of it.
///
/// # Errors
///
/// If the output is one of the inputs, or if the conversion itself failed — an input that could not
/// be read, a file that could not be written, a chapter list that is no plan.
pub fn run(mut args: ConvertArgs) -> Result<()> {
    let pieces = args.pieces;
    let split_after = std::mem::take(&mut args.split_after);
    let mut job = job(args);

    // Pieces are settled in front of the audio, from what the files state about themselves —
    // which is also the moment to say how long each of them will be. The two ways of choosing
    // the cuts cannot both be typed, so at most one of them is asked for here.
    let plan = if !split_after.is_empty() {
        Some(planned(&mut job, |job, layouts| {
            plan_cuts(job, layouts, &split_after)
        })?)
    } else if pieces.get() > 1 {
        Some(planned(&mut job, |job, layouts| {
            plan_pieces(job, layouts, pieces)
        })?)
    } else {
        None
    };
    let wanted = job.piece_starts.len() + 1;

    // A command line states one conversion, and it is held against itself while there is still
    // nothing on the disk to undo.
    refuse_collisions(std::slice::from_ref(&job))?;

    // The chapter count is said once, where it first stands: a plan somebody typed is settled in
    // front of the audio, so saying it there is a chance to stop the run rather than something
    // found out an hour of encoding later. A box counts the chapters of a file, so where the
    // book is cut into pieces it is each piece that is counted.
    let stated = planned_chapters(&job.options.chapter_mode);
    match &plan {
        Some(plan) => {
            for piece in &plan.pieces {
                warn_over_limit(piece.chapters);
            }
        }
        None => {
            if let Some(chapters) = stated {
                warn_over_limit(chapters);
            }
        }
    }

    let mut line = ProgressLine::default();
    let outcomes = run_convert(job, &mut |event| {
        line.show(event);

        std::ops::ControlFlow::Continue(())
    });
    // Whatever is said next — the files that were written, or why they were not — begins on a
    // line of its own.
    line.finish();
    let outcomes = outcomes.map_err(in_clock_time)?;

    // A plan nobody typed is settled by the conversion, and this is where it stands: the chapters
    // the file holds.
    if plan.is_none() && stated.is_none() {
        for outcome in &outcomes {
            warn_over_limit(outcome.report.chapters.len());
        }
    }
    for outcome in &outcomes {
        report(outcome);
    }
    // A cut is made where its chapter begins, and a chapter with no audio left begins nowhere.
    if let Some(warning) = fewer_pieces(outcomes.len(), wanted) {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

/// Plans the pieces `job` is cut into the way `cuts` chooses them from what its files state, puts
/// the cuts into the job, and says what they are.
fn planned(
    job: &mut ConvertJob,
    cuts: impl FnOnce(&ConvertJob, &[Option<Layout>]) -> Result<PiecePlan, PlanError>,
) -> Result<PiecePlan> {
    let layouts: Vec<Option<Layout>> = job
        .inputs
        .iter()
        .map(|input| probe_layout(input).ok())
        .collect();
    let plan = cuts(job, &layouts)?;
    job.piece_starts = plan.starts();
    announce(job, &plan);

    Ok(plan)
}

/// Says what the pieces of `job` are: the file each of them goes to, how long the headers say it
/// plays, and the chapters of the book it holds.
///
/// The chapters are said in the numbers the conversion gives them, which are the ones a cut is
/// typed in: a piece runs from its own first chapter to the one in front of the next piece's. A
/// skip can take chapters out of a file, and that is no reason to number the ones behind it anew.
fn announce(job: &ConvertJob, plan: &PiecePlan) {
    eprintln!("{} pieces:", plan.pieces.len());

    let lasts = plan
        .pieces
        .iter()
        .skip(1)
        .map(|piece| piece.first_chapter)
        .chain([plan.chapters]);
    for ((path, piece), last) in output_paths(job).iter().zip(&plan.pieces).zip(lasts) {
        let first = piece.first_chapter + 1;
        let chapters = if first == last {
            format!("chapter {first}")
        } else {
            format!("chapters {first}-{last}")
        };
        eprintln!(
            "  {}  ~{}  ({chapters})",
            path.display(),
            at_clock(piece.frames)
        );
    }
}

/// The job `args` states, with the output resolved.
fn job(args: ConvertArgs) -> ConvertJob {
    let ConvertArgs {
        inputs,
        output,
        skip_leading,
        skip_trailing,
        trim_pause_leading,
        trim_pause_each_chapter,
        add_pause_leading,
        add_pause_each_chapter,
        chapters,
        pieces: _,
        split_after: _,
        no_cover,
    } = args;

    let output = output.or_else(|| inputs.first().map(|first| default_output_path(first)));

    ConvertJob {
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
            skip_trailing: skip_trailing.to_samples_48k(),
            // Nothing on the command line states how many encoders to run, so a conversion takes
            // the machine as it finds it.
            workers: None,
        },
        write_cover: !no_cover,
        piece_starts: Vec::new(),
    }
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
