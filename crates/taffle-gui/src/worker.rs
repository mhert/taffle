//! The batch as it runs: books converting side by side, what they say while they do, and what is
//! left on the disk when one of them does not make it.
//!
//! There is no Qt in here, deliberately: a batch is jobs, threads and a channel, and the chrome
//! only ever hears about it through [`Event`]. So how a batch schedules, stops and tidies up is
//! held by ordinary unit tests rather than by somebody clicking.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use taffle::duration::RATE;

/// What the batch says while it runs; index is the job's position in the batch it was started
/// with.
#[derive(Debug)]
pub enum Event {
    /// The job has begun converting.
    Started {
        /// Which job, counted from the first one in the batch.
        index: usize,
    },
    /// Forwarded only when the whole second changed — the CLI's own progress rule.
    Progress {
        /// Which job, counted from the first one in the batch.
        index: usize,
        /// How much audio has gone into the file, in frames of one channel at 48 kHz.
        samples_done: u64,
    },
    /// The job is over, one way or the other.
    Finished {
        /// Which job, counted from the first one in the batch.
        index: usize,
        /// What it came to.
        result: Result<taffle::JobOutcome, BookFailure>,
    },
    /// Every job of the batch has been reported.
    BatchDone,
}

/// How a book did not make it, already classified for a row to show.
#[derive(Debug)]
pub enum BookFailure {
    /// The run was cancelled before or during this book.
    Cancelled,
    /// The rendered failure chain, every layer on one line — the CLI's own rendering.
    Failed(String),
}

/// How many books convert at once: a floor of 2 so a batch is parallel at all, a quarter of the
/// cores above that — each conversion brings its own encoder pool, and what a second book fills
/// is the gap the first one's serial decode leaves.
#[must_use]
pub fn concurrency_cap() -> usize {
    std::thread::available_parallelism().map_or(2, |cores| (cores.get() / 4).max(2))
}

/// A conversion as the batch calls it: the job to convert, the progress callback the engine
/// reports through, and what it came to. [`taffle::run_convert`] is one of these, and a test's
/// fake is another.
type Convert<'a> = &'a dyn Fn(
    taffle::ConvertJob,
    &mut dyn FnMut(taffle::Progress) -> ControlFlow<()>,
) -> Result<taffle::JobOutcome, taffle::JobError>;

/// Runs every job, at most `cap` at once. Workers hand their events over one internal channel and
/// the calling thread drains it through `deliver` — so delivery is single-threaded and in arrival
/// order, and `deliver` needs no thread-safety of its own. A raised `cancel` stops running jobs
/// between chunks and keeps waiting ones from starting; the partial file of a failed or cancelled
/// job is removed best-effort before it is reported. Returns once every job is reported and
/// [`Event::BatchDone`] was delivered.
pub fn run_batch<C, D>(
    jobs: &[taffle::ConvertJob],
    cap: usize,
    cancel: &AtomicBool,
    convert: C,
    mut deliver: D,
) where
    C: Fn(
            taffle::ConvertJob,
            &mut dyn FnMut(taffle::Progress) -> ControlFlow<()>,
        ) -> Result<taffle::JobOutcome, taffle::JobError>
        + Sync,
    D: FnMut(Event),
{
    // The job every worker takes next. A conversion runs for minutes, so which worker gets which
    // book is nobody's business — what matters is that a book is handed out once.
    let next = AtomicUsize::new(0);
    let (events, arrivals) = mpsc::channel();

    std::thread::scope(|scope| {
        // More workers than books would be threads with nothing to pull.
        for _ in 0..cap.min(jobs.len()) {
            let events = events.clone();
            let (next, convert) = (&next, &convert);
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::SeqCst);
                let Some(job) = jobs.get(index) else { break };

                convert_one(index, job, cancel, convert, &events);
            });
        }
        // The drain below ends when the last sender is gone, and this one is nobody's worker.
        drop(events);

        for event in arrivals {
            deliver(event);
        }
    });

    // Every worker has been joined by now, so this is the last word about the batch as well as
    // the last event of it.
    deliver(Event::BatchDone);
}

/// Converts the job at `index` and says on `events` what it came to.
///
/// A job the cancel flag arrives in front of is reported cancelled without being started: a batch
/// that was stopped still reports every book it was started with, so no row is left waiting for a
/// conversion that will never come.
///
/// A send that finds nobody listening is a drain that has gone away, which is a batch nobody is
/// waiting on any more — so saying any of this is best-effort, and there is nothing to do about a
/// word that arrives nowhere.
fn convert_one(
    index: usize,
    job: &taffle::ConvertJob,
    cancel: &AtomicBool,
    convert: Convert<'_>,
    events: &mpsc::Sender<Event>,
) {
    if cancel.load(Ordering::SeqCst) {
        let _ = events.send(Event::Finished {
            index,
            result: Err(BookFailure::Cancelled),
        });
        return;
    }
    let _ = events.send(Event::Started { index });

    // The second last reported, so a second is reported once however many chunks of audio it took
    // — the rule the command line's own progress line is written by.
    let mut reported = None;
    let outcome = convert(job.clone(), &mut |event| {
        if let taffle::Progress::Encoded { samples_done } = event {
            let second = samples_done / u64::from(RATE);
            if reported != Some(second) {
                reported = Some(second);
                let _ = events.send(Event::Progress {
                    index,
                    samples_done,
                });
            }
        }

        if cancel.load(Ordering::SeqCst) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });

    let result = outcome.map_err(|error| {
        // A conversion that failed part-way leaves the file it was writing behind, a cancelled one
        // included, and half a book is no book. Removing it is best-effort on purpose: what is
        // reported is how the conversion went, not how the tidying after it went.
        if let Some(output) = &job.output {
            let _ = std::fs::remove_file(output);
        }

        classify(&error)
    });
    let _ = events.send(Event::Finished { index, result });
}

/// What a row shows for a job that did not make it.
///
/// Being stopped is not a book that failed — it is the one failure a person asked for — so it is
/// told apart here rather than rendered as a chain nobody needs to read.
fn classify(error: &taffle::JobError) -> BookFailure {
    if matches!(
        error,
        taffle::JobError::Convert(taffle::ConvertError::Cancelled)
    ) {
        return BookFailure::Cancelled;
    }

    BookFailure::Failed(chain(error))
}

/// Every layer of `error` on one line, joined by `": "`.
///
/// This is what the command line prints, written out here because the rendering it uses is
/// `anyhow`'s and `anyhow` stays in the command line.
fn chain(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut below = error.source();
    while let Some(layer) = below {
        line.push_str(": ");
        line.push_str(&layer.to_string());
        below = layer.source();
    }

    line
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    /// A job naming `input` and `output` — resolved output, engine defaults, no cover.
    fn job(input: &str, output: &str) -> taffle::ConvertJob {
        taffle::ConvertJob {
            inputs: vec![input.into()],
            output: Some(output.into()),
            options: taffle::Conversion::default(),
            write_cover: false,
        }
    }

    /// The outcome of a job that "converted": every field is public, none read from a disk.
    fn ok_outcome(job: &taffle::ConvertJob) -> taffle::JobOutcome {
        taffle::JobOutcome {
            taf_path: job.output.clone().expect("resolved output"),
            cover_path: None,
            cover_error: None,
            report: taffle::ConversionReport {
                chapters: vec![],
                duration: std::time::Duration::ZERO,
                cover: None,
                audio_id: taffle::AudioId::new(1),
            },
        }
    }

    #[test]
    fn a_batch_is_parallel_on_any_machine() {
        // A single core, and a machine that will not say how many it has, still convert two books
        // at once: the floor is what makes a batch a batch rather than a queue.
        assert!(concurrency_cap() >= 2, "{} at once", concurrency_cap());
    }

    #[test]
    fn no_more_than_cap_run_at_once_and_every_job_finishes() {
        let live = AtomicUsize::new(0);
        let high = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel();
        let jobs: Vec<_> = (0..6)
            .map(|at| job(&format!("in{at}.mp3"), &format!("out{at}.taf")))
            .collect();

        run_batch(
            &jobs,
            2,
            &AtomicBool::new(false),
            |job, _progress| {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                live.fetch_sub(1, Ordering::SeqCst);
                Ok(ok_outcome(&job))
            },
            |event| tx.send(event).unwrap(),
        );

        assert!(
            high.load(Ordering::SeqCst) <= 2,
            "{} ran at once",
            high.load(Ordering::SeqCst)
        );
        let events: Vec<Event> = rx.try_iter().collect();
        let indices = |of: fn(&Event) -> Option<usize>| {
            let mut seen: Vec<usize> = events.iter().filter_map(of).collect();
            seen.sort_unstable();
            seen
        };
        let all = (0..6).collect::<Vec<_>>();
        assert_eq!(
            indices(|event| match event {
                Event::Started { index } => Some(*index),
                _ => None,
            }),
            all,
            "every job says which one of them started"
        );
        assert_eq!(
            indices(|event| match event {
                Event::Finished { index, result } => {
                    assert!(result.is_ok());
                    Some(*index)
                }
                _ => None,
            }),
            all
        );
        assert!(matches!(events.last(), Some(Event::BatchDone)));
    }

    #[test]
    fn progress_is_forwarded_on_whole_second_changes_only() {
        let (tx, rx) = mpsc::channel();
        run_batch(
            &[job("a.mp3", "a.taf")],
            1,
            &AtomicBool::new(false),
            |job, progress| {
                for samples_done in [100, 47_999, 48_000, 48_500, 96_000] {
                    let _ = progress(taffle::Progress::Encoded { samples_done });
                }
                Ok(ok_outcome(&job))
            },
            |event| tx.send(event).unwrap(),
        );
        let seen: Vec<u64> = rx
            .try_iter()
            .filter_map(|event| match event {
                Event::Progress {
                    index,
                    samples_done,
                } => {
                    // What is said is said about the job it happened to, so the bar it moves is
                    // that job's row and no other.
                    assert_eq!(index, 0, "the one job of this batch");
                    Some(samples_done)
                }
                _ => None,
            })
            .collect();
        // Second 0 is shown once, the way the CLI's own line shows it, then only whole-second
        // changes pass.
        assert_eq!(seen, [100, 48_000, 96_000]);
    }

    #[test]
    fn cancel_stops_the_running_and_the_waiting() {
        let cancel = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel();
        // A cancelled job has its output removed, so the outputs are named under a directory of
        // this test's own: named relatively they would be whatever a.taf and b.taf stand for in
        // the directory `cargo test` runs in, and the run would delete them.
        let dir = tempfile::tempdir().expect("temp dir");
        let output = |stem: &str| {
            dir.path()
                .join(stem)
                .to_str()
                .expect("utf-8 temp path")
                .to_owned()
        };
        run_batch(
            &[
                job("a.mp3", &output("a.taf")),
                job("b.mp3", &output("b.taf")),
            ],
            1,
            &cancel,
            |_job, progress| {
                calls.fetch_add(1, Ordering::SeqCst);
                cancel.store(true, Ordering::SeqCst);
                // The next report finds the flag raised and is told to stop, the way a real
                // conversion is between chunks.
                assert!(progress(taffle::Progress::Finalizing).is_break());
                Err(taffle::JobError::Convert(taffle::ConvertError::Cancelled))
            },
            |event| tx.send(event).unwrap(),
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the waiting job must never start"
        );
        let cancelled: Vec<usize> = rx
            .try_iter()
            .filter_map(|event| match event {
                Event::Finished {
                    index,
                    result: Err(BookFailure::Cancelled),
                } => Some(index),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled, [0, 1]);
    }

    #[test]
    fn the_partial_file_of_a_failed_job_is_removed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output = dir.path().join("book.taf");
        let (tx, rx) = mpsc::channel();
        run_batch(
            &[job("a.mp3", output.to_str().expect("utf-8 temp path"))],
            1,
            &AtomicBool::new(false),
            |job, _progress| {
                let path = job.output.as_ref().expect("resolved output");
                std::fs::write(path, b"partial").expect("writing the partial file");
                Err(taffle::JobError::Convert(taffle::ConvertError::Io(
                    std::io::Error::other("boom"),
                )))
            },
            |event| tx.send(event).unwrap(),
        );
        assert!(!output.exists(), "the unfinished file must not stay behind");
        let failed = rx.try_iter().any(|event| {
            matches!(event,
                Event::Finished { result: Err(BookFailure::Failed(message)), .. }
                    if message.contains("boom"))
        });
        assert!(failed, "the rendered chain must reach the row");
    }
}
